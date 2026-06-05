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
//! `Application.onCreate()` → `Activity.createMainActivity(String, J, String)` →
//! `Activity.onCreate(Bundle)` — driving the launcher Activity to `onCreate` for a pure-Java APK.
//! The recipe steps are encoded as typed constants
//! ([`STEP1_CREATE_APPLICATION`] … [`STEP5_ACTIVITY_ON_CREATE`]).
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
//! ## Steps 4–5 (driven against Eclipse-owned handles, 2026-06-05)
//! Steps **4–5** — `Activity.createMainActivity(String, jlong, String)→Activity` and
//! `Activity.onCreate(Bundle)` — are now driven. The `jlong` is the **same Eclipse-owned
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

use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::OnceLock;

use jni::errors::LogErrorAndDefault;
use jni::objects::{JClass, JIntArray, JObject, JString};
use jni::refs::Reference;
use jni::signature::{FieldSignature, JavaType, Primitive};
use jni::strings::JNIStr;
use jni::sys::{jboolean, jint, jlong, jshort};
use jni::vm::JavaVM;
use jni::{jni_sig, jni_str, Env, EnvUnowned, JValue, NativeMethod};

use crate::runtime::Vm;

pub mod paint_registry;
pub mod theme_registry;
pub mod view_registry;
pub mod window_registry;
pub mod xml_registry;

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

/// `Res_value.dataType` for a string-pool reference (`TYPE_STRING`); its `data` is a value-pool index.
const RES_VALUE_TYPE_STRING: u8 = 0x03;
/// The single asset cookie Eclipse reports (one APK). `loadResourceValue` returns it on success.
const ECLIPSE_ASSET_COOKIE: jint = 1;

// === ATL TypedArray ABI: the per-attribute window layout retrieveAttributes writes ==============
//
// 2026-06-05: ATL's `retrieveAttributes` is **ATL-specific**, not the stock AOSP native — its JNI
// signature carries an extra `int` (the attrs-array length) that AOSP's `nativeRetrieveAttributes`
// does not have. So the AOSP-documented `STYLE_*` offsets do NOT necessarily apply. ATL's
// `TypedArray.java`/`AssetManager.java` are on the cyber-safeguard denylist (asset/res source), so
// the window layout was determined **empirically** from the dev-host run (a benign, allowed
// observation), NOT by reading that source:
//
//   • Writing a distinct sentinel into each of the 6 ints of a matched window and observing which
//     value `TypedArray.getInteger` read back as the "type" showed the **TYPE byte is at offset 1**
//     (the framework reported `type=0xb0`, the sentinel written at window+1), NOT AOSP's offset 0.
//   • The DATA word's offset was nailed down by sweeping which slot carries it: writing the real
//     `Res_value.data` into ONLY slot 3 (with slots 2/0/4/5 left at the framework's zero pre-fill)
//     made BOTH `PackageParser`'s integer attributes resolve (no `Can't convert to integer`) AND
//     `TypedArray.getString` resolve `<activity android:name>` — clearing the "`<activity> does not
//     specify android:name` → System.exit(1)" stop and advancing the lifecycle to step 1
//     `Context.createApplication`. Sweeping DATA into each slot in turn, ONLY slot 3 made getString
//     resolve; slot 2 (the earlier guess) satisfied integers but left getString returning null. So
//     **DATA is at offset 3** — the one layout that satisfies every typed accessor. (The earlier note
//     "DATA@2" was an integer-only coincidence: the integer path tolerates 2 or 3, the string path
//     requires 3.)
//
// So ATL's window is `[?, TYPE(1), ?, DATA(3), ?, ?]` with `STYLE_NUM_ENTRIES = 6` (the 48-int zero
// pre-fill the framework hands us for an 8-attribute manifest styleable confirms the 6-int stride).
// The remaining 4 slots (offset 0, 2, 4, 5 — cookie/resource-id/etc.) are left at the framework's
// own zero pre-fill: their exact ATL offsets are not yet confirmed and writing a wrong value there is
// worse than the neutral zero default. A `TYPE_STRING` value at DATA@3 is the string-pool index; the
// framework's `TypedArray.getString` resolves it via the XmlBlock string pool (cookie slot = 0 routes
// to `mXml.getPooledString(data)`, satisfied by the already-bound `nativeGetAttributeStringValue` /
// XmlDocument string pool — no new native is needed, confirmed by the run: no `No implementation
// found` surfaced and the activity name resolved entirely in Java).
//
// THE ONE ABI ASSUMPTION (faithful): the empirically-confirmed `STYLE_NUM_ENTRIES = 6` stride with
// TYPE@1 / DATA@3. A regression here would mis-place the entries; the run-derived offsets are pinned
// by the unit tests below so a transcription change fails loudly.

/// ATL `TypedArray` per-attribute window stride in `outValues` (empirically confirmed, see above).
const STYLE_NUM_ENTRIES: usize = 6;
/// Offset of the `TypedValue.TYPE_*` byte within an attribute's window (ATL = 1, run-confirmed).
const STYLE_TYPE: usize = 1;
/// Offset of the `Res_value.data` word within an attribute's window (ATL = 3, run-confirmed 2026-06-05
/// — the ONE slot that makes both `getInteger` and `getString` resolve). For a `TYPE_STRING` this is
/// the XmlBlock string-pool index the framework's `getString` resolves via the XML string pool.
const STYLE_DATA: usize = 3;
/// `TypedValue.TYPE_NULL` — "no value" (the framework then uses the attribute's default). Written
/// into a requested attribute's `STYLE_TYPE` slot when that id is absent from the current tag.
const TYPE_NULL: i32 = 0;

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
/// `TypedValue.TYPE_*` code and the data word. `None` for a requested id absent from the tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TypedEntry {
    /// `Res_value.dataType` (== `TypedValue.TYPE_*`).
    value_type: i32,
    /// `Res_value.data` (for a string, the XmlBlock string-pool index).
    data: i32,
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
                Some(TypedEntry {
                    value_type: i32::from(attr.value_type),
                    data: u32_to_i32(attr.value_data),
                })
            })
            .collect()
    })
    .unwrap_or_else(|_| vec![None; ids.len()])
}

/// Reinterpret a `u32` `Res_value.data` word as the `i32` the TypedArray `int[]` stores (bit-for-bit;
/// the framework reads it back as the same 32 bits). `as` would also work, but `from_ne_bytes` makes
/// the bit-preservation explicit and lint-clean.
fn u32_to_i32(v: u32) -> i32 {
    i32::from_ne_bytes(v.to_ne_bytes())
}

/// Fill the framework-allocated `TypedArray` output buffers from `entries` (one per requested
/// attribute, in request order): each `Some` writes the run-confirmed [`STYLE_TYPE`]/[`STYLE_DATA`]
/// slots of its [`STYLE_NUM_ENTRIES`]-wide window (the rest stay at the framework's zero pre-fill),
/// each `None` writes `TYPE_NULL` into its window's `STYLE_TYPE` slot; `outIndices[0]` is set to the
/// number of `Some` entries, followed by their 1-based request positions.
///
/// `out_values`/`out_indices` are the raw `jlong` pointers the framework passed; `0` means the
/// framework provided no buffer and that buffer is skipped (no write). The writes are bounded to the
/// AOSP-sized regions: offsets `< n * STYLE_NUM_ENTRIES` for `outValues` and `<= n` for `outIndices`,
/// where `n == entries.len()`.
///
/// # Safety
/// 2026-06-05: this performs raw `*mut i32` writes, justified by the ATL `TypedArray` ABI: the
/// framework's `TypedArray` allocates `outValues` with `attrs.length * STYLE_NUM_ENTRIES` ints and
/// `outIndices` with `attrs.length + 1` ints (the 6-int stride confirmed by the framework's 48-int
/// zero pre-fill for an 8-attribute styleable), and passes their base addresses as these two
/// `jlong`s; `n = entries.len()` here IS `attrs.length` (`entries` is built one-per-`ids` entry, and
/// `ids.len()` is `attrs.len()` from `JIntArray::len`). For `outValues` every written offset is
/// `attr * STYLE_NUM_ENTRIES + slot` with `attr < n` and `slot ∈ {STYLE_TYPE, STYLE_DATA} <
/// STYLE_NUM_ENTRIES`, hence `< n * STYLE_NUM_ENTRIES`. For `outIndices` the written offsets are `0`
/// (the count) and `1..=changed` where `changed <= n`, hence `<= n`. Both are strictly inside the
/// framework's allocation — no out-of-bounds access. A `0` pointer is treated as "no buffer" and
/// never dereferenced. The one ABI assumption (documented at the `STYLE_*` constants) is the
/// empirically-confirmed `STYLE_NUM_ENTRIES = 6` / TYPE@1 / DATA@3 layout. Each `i32` is written to a
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
                    // SAFETY: window + STYLE_DATA <= window + (STYLE_NUM_ENTRIES-1) <
                    // (attr+1)*STYLE_NUM_ENTRIES <= n*STYLE_NUM_ENTRIES = the framework's outValues
                    // int-count (see the fn-level # Safety). `base` is non-null (checked) and points
                    // at that framework-owned, 4-byte-aligned int[]. Only the run-confirmed TYPE and
                    // DATA slots are written; the others stay at the framework's zero pre-fill (the
                    // neutral default — their exact ATL offsets are not yet confirmed).
                    unsafe {
                        base.add(window + STYLE_TYPE).write(e.value_type);
                        base.add(window + STYLE_DATA).write(e.data);
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
        match theme_registry::with_theme(theme, |t| t.styles.push(style_res)) {
            Ok(()) => tracing::debug!(
                target: "android.content.res.AssetManager",
                theme,
                style_res,
                force,
                "AssetManager.applyThemeStyle: recorded style on non-GTK theme"
            ),
            Err(e) => tracing::debug!(
                target: "android.content.res.AssetManager",
                theme,
                style_res,
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
        let src_styles = theme_registry::with_theme(source, |t| t.styles.clone());
        match src_styles {
            Ok(styles) => {
                if let Err(e) = theme_registry::with_theme(dest, |t| t.styles = styles) {
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
/// int[] attrs, int length, long outValues, long outIndices)` → write `TYPE_NULL` for every
/// requested attribute (the theme resolves no styled values for a fresh View — it uses defaults).
///
/// JNI ABI: an INSTANCE native returning void. `outValues`/`outIndices` are the framework's
/// `TypedArray` off-heap buffers (same ABI as [`asset_manager_retrieve_attributes`]). 2026-06-05:
/// minimal-and-sound — a freshly constructed View has no theme-resolved style values, so this writes
/// `TYPE_NULL` into each requested attribute's window and `outIndices[0] = 0` (nothing changed),
/// which makes the framework's `TypedArray` use each attribute's built-in default. That is exactly
/// AOSP's behavior when a styleable attribute is unset — not a value fake. Theme-driven resolution
/// against `resources.arsc` is a later increment (would read [`theme_registry`] styles + the ARSC
/// bag); it is not needed to construct the launcher's default Views.
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
    _parser: jlong,
    _def_style_attr: jint,
    _def_style_res: jint,
    attrs: JIntArray<'local>,
    _length: jint,
    out_values: jlong,
    out_indices: jlong,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        // Size the all-default fill to the requested attribute count. A null attrs array means
        // nothing to fill; still write outIndices[0]=0 so the framework reads a defined count.
        let n = if attrs.is_null() { 0 } else { attrs.len(env)? };
        // All-`None`: TYPE_NULL per requested attribute (defaults used), outIndices[0] = 0. Reuses
        // the bounds-proven writer (writes only < n*STYLE_NUM_ENTRIES / <= n; a 0 ptr is skipped).
        let entries = vec![None; n];
        fill_typed_array(out_values, out_indices, &entries);
        tracing::debug!(
            target: "android.content.res.AssetManager",
            theme,
            attrs = n,
            "AssetManager.applyStyle: wrote TYPE_NULL defaults for theme styled attributes (non-GTK)"
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
    ];
    // SAFETY: `class` is the loaded android/content/res/AssetManager; `methods` hold valid fn
    // pointers whose signatures match the class's `native` declarations (`init` verified against
    // AssetManager.java line 779; `native_setApkAssets` bound signature-only from the ART-reported
    // signature `([Ljava/lang/Object;I)V` — AssetManager is denylisted, 2026-06-05).
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/content/res/AssetManager",
        "registered Eclipse's non-GTK backing for AssetManager.init + native_setApkAssets + setConfiguration + openXmlAssetNative + retrieveAttributes + newTheme + applyThemeStyle + copyTheme + applyStyle + getResourceName + loadResourceValue"
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

// `static native int nativeGetLineNumber(long state)` — the current node's source line number (used
// only by `getPositionDescription` for error messages). JNI descriptor `(J)I` (run log 2026-06-05).
// Eclipse's axml reader does not track source line numbers, so this honestly returns -1 ("unknown"),
// which AOSP's XmlResourceParser uses when a line is unavailable.
const XML_BLOCK_GET_LINE_NUMBER_NAME: &JNIStr = jni_str!("nativeGetLineNumber");
const XML_BLOCK_GET_LINE_NUMBER_SIG: &JNIStr = jni_str!("(J)I");

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
        // SAFETY: `xml_block_get_line_number` matches the paired `(J)I` signature as a static native.
        unsafe {
            NativeMethod::from_raw_parts(
                XML_BLOCK_GET_LINE_NUMBER_NAME,
                XML_BLOCK_GET_LINE_NUMBER_SIG,
                xml_block_get_line_number as *mut std::ffi::c_void,
            )
        },
    ];
    // SAFETY: `class` is the loaded android/content/res/XmlBlock; `methods` hold valid fn pointers
    // whose signatures match the class's `native` declarations (from the ART-reported signatures,
    // 2026-06-05).
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/content/res/XmlBlock",
        "registered Eclipse's non-GTK backing for XmlBlock parser natives (nativeCreateParseState/nativeNext/nativeDestroyParseState/nativeGetName/nativeDestroy/nativeGetLineNumber)"
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

/// `View.native_setPadding(long widget, int left, int top, int right, int bottom)` → validate the
/// view handle; no-op (2026-06-05).
///
/// JNI ABI: an INSTANCE native returning void, so the parameters are
/// `(EnvUnowned, JObject this, jlong widget, jint left, jint top, jint right, jint bottom)`. Padding
/// is layout data Eclipse does not act on yet (no layout/measure/draw without the deferred surface),
/// so this validates the `widget` handle through the bounds+generation-checked [`view_registry`]
/// (a stale/fabricated handle is logged + ignored, never UB) and otherwise no-ops. Binding it lets
/// the View constructor proceed; the padding can be recorded once layout lands.
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
        if let Err(e) = view_registry::with_view(widget, |_v| ()) {
            tracing::debug!(
                target: "android.view.View",
                widget,
                error = %e,
                "View.native_setPadding: invalid view handle (ignored)"
            );
        } else {
            tracing::trace!(
                target: "android.view.View",
                widget, left, top, right, bottom,
                "View.native_setPadding: validated handle, no-op (layout deferred)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `View.native_setLayoutParams(long widget, int width, int height, int gravity, float weight,
/// int leftMargin, int topMargin, int rightMargin, int bottomMargin)` → validate handle; no-op
/// (layout deferred, 2026-06-05).
///
/// JNI ABI: an INSTANCE native returning void (`View.java` line 1167). Layout sizing/margins/gravity
/// are applied once real layout lands (deferred); for now this validates the `widget` handle through
/// the bounds+generation-checked [`view_registry`] (a bad handle is logged + ignored, never UB) and
/// no-ops, letting `ViewGroup.addView` proceed.
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
    _width: jint,
    _height: jint,
    _gravity: jint,
    _weight: f32,
    _left_margin: jint,
    _top_margin: jint,
    _right_margin: jint,
    _bottom_margin: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = view_registry::with_view(widget, |_v| ()) {
            tracing::debug!(
                target: "android.view.View",
                widget,
                error = %e,
                "View.native_setLayoutParams: invalid view handle (ignored)"
            );
        } else {
            tracing::trace!(
                target: "android.view.View",
                widget,
                "View.native_setLayoutParams: validated handle, no-op (layout deferred)"
            );
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
    ];
    // SAFETY: `class` is the loaded android/view/View; `methods` hold valid fn pointers whose
    // signatures match the class's `native` declarations (verified against View.java lines 1166/1310,
    // 2026-06-05).
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/view/View",
        "registered Eclipse's non-GTK backing for View.native_constructor + native_setPadding + native_setLayoutParams + native_requestLayout"
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
    ];
    // SAFETY: `class` is the loaded android/view/ViewGroup; the fn pointer's signature matches its
    // `native_addView` declaration (verified against ViewGroup.java line 186, 2026-06-05).
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/view/ViewGroup",
        "registered Eclipse's non-GTK backing for ViewGroup.native_addView"
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
    ];
    // SAFETY: `class` is the loaded android/graphics/Paint; the fn pointer's signature matches its
    // `native_create` declaration (from the ART-reported signature, 2026-06-05).
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/graphics/Paint",
        "registered Eclipse's non-GTK backing for Paint.native_create"
    );
    Ok(())
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

/// Bind Eclipse's own (non-GTK) backing for `android.widget.TextView`'s peer natives.
///
/// `native_constructor` (TextView.java line 89, same `(Landroid/content/Context;Landroid/util/
/// AttributeSet;)J` signature as View's) reuses the class-agnostic [`view_native_constructor`], which
/// records the receiver's actual class (`android.widget.TextView`) in [`view_registry`]. Registered
/// before step 4, alongside the View/Window natives.
///
/// # Safety / soundness
/// `register_native_methods` is `unsafe`: the fn pointer must match the declared JNI signature. It
/// does — [`view_native_constructor`] is written to that exact descriptor. The body is
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
    ];
    // SAFETY: `class` is the loaded android/widget/TextView; the fn pointer's signature matches its
    // `native_constructor` declaration (verified against TextView.java line 89, 2026-06-05).
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/widget/TextView",
        "registered Eclipse's non-GTK backing for TextView.native_constructor"
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
    ];
    // SAFETY: `class` is the loaded android/view/Window; `methods` hold valid fn pointers whose
    // signatures match the class's `native` declarations (verified against Window.java lines 184–188,
    // 2026-06-05).
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/view/Window",
        "registered Eclipse's non-GTK backing for Window.set_jobject + set_title + set_layout + set_widget_as_root"
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
/// The `android.app.Activity` class (internal name) — hosts the `static` `createMainActivity` entry
/// point (step 4) and the instance `onCreate(Bundle)` (step 5).
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
/// 2026-06-05: the driver now attempts the full recipe 1–5. It reaches
/// [`ApplicationOnCreate`](LifecycleProgress::ApplicationOnCreate) (steps 1–3 proven), then drives
/// step 4 (`Activity.createMainActivity`) and step 5 (`Activity.onCreate`); reaching the latter is
/// [`ActivityOnCreate`](LifecycleProgress::ActivityOnCreate). Step 4 onward consume the `jlong`
/// window handle, which the Window/View natives **dereference** (unlike steps 1–3, which only store
/// it) — those natives are bound non-GTK against [`window_registry`]/[`view_registry`] as the
/// dev-host run surfaces them.
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
    /// `onCreate` reached — the increment's milestone.
    ActivityOnCreate,
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
/// # Steps 4–5
/// Drives step 4 (`Activity.createMainActivity(launcher_activity, window, null)`) and step 5
/// (`Activity.onCreate(null Bundle)`) after steps 1–3. `launcher_activity` is the dotted Java class
/// name of the manifest's MAIN/LAUNCHER Activity. The `jlong` window handle is the same Eclipse-owned
/// [`window_registry`] handle steps 1–3 received; step 4's Window/View natives dereference it (bound
/// non-GTK against [`window_registry`]/[`view_registry`]). Returns
/// [`LifecycleProgress::ActivityOnCreate`] on success; if a step's native is not yet bound the run's
/// `No implementation found` line names the next one to add (the dev-host discovery loop).
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
    // Bind android.content.res.XmlBlock's parser natives on its own class — once openXmlAssetNative
    // returns a real block handle, the framework walks it via XmlBlock (reading AndroidManifest.xml
    // during Context.<clinit>), so these must be bound before step 1.
    register_xml_block_natives(env)?;
    // Bind android.os.Environment.native_get_app_data_dir on its own class — the framework queries
    // external storage early in init (`getExternalStorageDirectory`), so this must be bound before
    // step 1.
    register_environment_natives(env)?;
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
    // Bind android.view.ViewGroup's tree-wiring natives on its own class — setContentView's
    // LayoutInflater wires children via ViewGroup.addView during step 5, so this must be bound before
    // step 4. Bound non-GTK against view_registry (records the tree edges).
    register_view_group_natives(env)?;
    // Bind android.graphics.Paint's natives on its own class — the View hierarchy's TextPaint/Paint
    // construct during step 5's setContentView, so this must be bound before step 4. Bound non-GTK
    // against paint_registry (config only; no drawing).
    register_paint_natives(env)?;

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
    Ok(LifecycleProgress::ActivityOnCreate)
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
        // The step-4/5 Activity class internal (slashed) name used by find_class.
        assert_eq!(ACTIVITY_CLASS.to_str(), "android/app/Activity");
        assert_eq!(STEP4_CREATE_MAIN_ACTIVITY.class, "android/app/Activity");
        assert_eq!(STEP5_ACTIVITY_ON_CREATE.class, "android/app/Activity");
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
        assert_eq!(CHAR_SEQUENCE_SIG.to_str(), "Ljava/lang/CharSequence;");
        assert_eq!(RES_VALUE_TYPE_STRING, 0x03);
        assert_eq!(ECLIPSE_ASSET_COOKIE, 1);
        // Pin the EMPIRICALLY-CONFIRMED ATL TypedArray window layout retrieveAttributes writes
        // against (run-derived 2026-06-05: stride 6, TYPE@1, DATA@3 — NOT the AOSP-documented
        // TYPE@0/DATA@1, and NOT the earlier integer-only guess DATA@2). DATA@3 is the ONE slot that
        // makes both `getInteger` (PackageParser integers) and `getString` (`<activity android:name>`)
        // resolve; a stride/offset regression (which would re-break the activity-name getString and
        // mis-place TypedValue entries) fails loudly.
        assert_eq!(STYLE_NUM_ENTRIES, 6);
        assert_eq!(STYLE_TYPE, 1);
        assert_eq!(STYLE_DATA, 3);
        assert_eq!(TYPE_NULL, 0);
    }

    #[test]
    fn fill_typed_array_writes_exact_bounds_values_and_indices() {
        // SOUNDNESS guard for the raw-pointer writes in retrieveAttributes (no VM needed): the
        // writes must stay strictly inside the AOSP-sized buffers (n * STYLE_NUM_ENTRIES ints for
        // outValues, n + 1 for outIndices), write a full value window for each found attribute,
        // TYPE_NULL for each absent one, and pack outIndices[0]=count + the 1-based positions.
        //
        // Sentinel-bracketed buffers detect any out-of-bounds write: a leading + trailing guard cell
        // must keep its sentinel. entries: [found, absent, found, absent] (mixed).
        let entries = [
            Some(TypedEntry {
                value_type: 0x03,
                data: 0x18,
            }),
            None,
            Some(TypedEntry {
                value_type: 0x10,
                data: 0x2a,
            }),
            None,
        ];
        let n = entries.len();
        let vals_len = n * STYLE_NUM_ENTRIES;
        let idx_len = n + 1;

        let mut values = vec![-1i32; vals_len + 2]; // [guard][n*6 values][guard]
        let mut indices = vec![-1i32; idx_len + 2]; // [guard][n+1 indices][guard]

        let v_ptr = values[1..1 + vals_len].as_mut_ptr() as jlong;
        let i_ptr = indices[1..1 + idx_len].as_mut_ptr() as jlong;
        fill_typed_array(v_ptr, i_ptr, &entries);

        // Guards untouched (no underflow / overflow write).
        assert_eq!(values[0], -1, "outValues underflow guard");
        assert_eq!(values[vals_len + 1], -1, "outValues overflow guard");
        assert_eq!(indices[0], -1, "outIndices underflow guard");
        assert_eq!(indices[idx_len + 1], -1, "outIndices overflow guard");

        // Found attributes (0 and 2): only the run-confirmed TYPE@1 and DATA@3 slots are written;
        // the other 4 slots stay at the caller value (the framework's zero pre-fill in real use).
        for (attr, e) in [(0usize, &entries[0]), (2usize, &entries[2])] {
            let win = 1 + attr * STYLE_NUM_ENTRIES;
            let e = e.unwrap();
            assert_eq!(values[win + STYLE_TYPE], e.value_type, "STYLE_TYPE @1");
            assert_eq!(values[win + STYLE_DATA], e.data, "STYLE_DATA @3");
            for slot in 0..STYLE_NUM_ENTRIES {
                if slot != STYLE_TYPE && slot != STYLE_DATA {
                    assert_eq!(values[win + slot], -1, "unwritten slot untouched");
                }
            }
        }
        // Absent attributes (1 and 3): only STYLE_TYPE @1 = TYPE_NULL written.
        for attr in [1usize, 3usize] {
            let win = 1 + attr * STYLE_NUM_ENTRIES;
            assert_eq!(values[win + STYLE_TYPE], TYPE_NULL, "absent → TYPE_NULL @1");
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
    fn fill_typed_array_null_pointers_are_a_no_op() {
        // A 0 ("no buffer") pointer for either output must be skipped — never dereferenced. A
        // non-empty entries slice ensures the loop body would run if the guard were missing.
        let entries = [Some(TypedEntry {
            value_type: 0x03,
            data: 1,
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
        // nativeGetLineNumber: `(J)I`, returns -1 (axml does not track source lines).
        assert_eq!(
            XML_BLOCK_GET_LINE_NUMBER_NAME.to_str(),
            "nativeGetLineNumber"
        );
        assert_eq!(XML_BLOCK_GET_LINE_NUMBER_SIG.to_str(), "(J)I");
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
        // TextView re-declares native_constructor (same signature); pin its class internal name.
        assert_eq!(TEXT_VIEW_CLASS.to_str(), "android/widget/TextView");
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
    }

    #[test]
    fn paint_native_name_sig_and_class_match_art_reported() {
        // Pin android.graphics.Paint.native_create's class, method name, and JNI descriptor against
        // the exact signature ART reported missing (run log 2026-06-05): a transcription regression
        // would make RegisterNatives throw NoSuchMethodError at boot. Host-independent constants.
        assert_eq!(PAINT_CLASS.to_str(), "android/graphics/Paint");
        assert_eq!(PAINT_NATIVE_CREATE_NAME.to_str(), "native_create");
        assert_eq!(PAINT_NATIVE_CREATE_SIG.to_str(), "()J");
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
