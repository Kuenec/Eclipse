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
//! Window natives), which is deferred — that step (and its dev-host-gated Window natives) consumes
//! the same handle, so the slot is intentionally not freed during the run.
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
use jni::objects::{JClass, JIntArray, JObject, JString};
use jni::refs::Reference;
use jni::signature::{FieldSignature, JavaType, Primitive};
use jni::strings::JNIStr;
use jni::sys::{jboolean, jint, jlong};
use jni::vm::JavaVM;
use jni::{jni_sig, jni_str, Env, EnvUnowned, JValue, NativeMethod};

use crate::runtime::Vm;

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
    ];
    // SAFETY: `class` is the loaded android/content/res/AssetManager; `methods` hold valid fn
    // pointers whose signatures match the class's `native` declarations (`init` verified against
    // AssetManager.java line 779; `native_setApkAssets` bound signature-only from the ART-reported
    // signature `([Ljava/lang/Object;I)V` — AssetManager is denylisted, 2026-06-05).
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/content/res/AssetManager",
        "registered Eclipse's non-GTK backing for AssetManager.init + native_setApkAssets + setConfiguration + openXmlAssetNative + retrieveAttributes"
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
    ];
    // SAFETY: `class` is the loaded android/content/res/XmlBlock; `methods` hold valid fn pointers
    // whose signatures match the class's `native` declarations (from the ART-reported signatures,
    // 2026-06-05).
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/content/res/XmlBlock",
        "registered Eclipse's non-GTK backing for XmlBlock parser natives (nativeCreateParseState/nativeNext/nativeDestroyParseState/nativeGetName/nativeDestroy)"
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
/// handle that the Window natives **dereference**, and those (non-GTK) Window natives are not yet
/// bound — see the module docs. Steps 1–3 only *store* the handle, so the real Eclipse-owned
/// registry handle from [`window_registry::allocate`] is passed and the slot is left allocated for
/// step 4; driving step 4 onward is unblocked by the framework/Surface design (component-map F).
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
}
