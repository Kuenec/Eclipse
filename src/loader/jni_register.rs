//! Live wiring of the pre-loaded-lib `Java_*` discovery-gap fix (2026-06-11).
//!
//! ## The gap
//! Eclipse pre-loads a JNI lib through its OWN mmap loader (not ART's `dlopen`), so the lib is not in
//! ART's `libraries_` — ART's lazy native resolution (`dlsym(handle, "Java_…")`) has nothing to dlsym,
//! and the lib's natives surface as `UnsatisfiedLinkError`. Observed on the real v2.721.1108 boot:
//! `com.roblox.client.JNIAAssetManagerSetup.initNative` (aborts `Application.onCreate`),
//! `com.roblox.universalapp.logging.JNILoggingProtocol.nativeGetTimestamp`/`nativeLogEvent`, and
//! `com.github.luben.zstd.ZstdInputStreamNoFinalizer.recommendedDInSize`.
//!
//! ## The fix
//! After a lib is pre-loaded, Eclipse `RegisterNatives` its natives against ART — binding each declared
//! native to the implementation address in Eclipse's mapped image (what ART's `dlsym` would have
//! returned). This module is the LIVE half; [`super::jni_mangle`] is the deterministic demangler half.
//!
//! ## General (reflection) primary, curated fallback
//! [`register_all_preloaded_natives`] is the GENERAL fix: enumerate ALL of the lib's `Java_*` exports
//! ([`super::engine::LoadedEngine::java_native_exports`]) → [`super::jni_mangle::demangle`] each →
//! reflect every declaring class's declared `native` methods for their full JNI signatures →
//! `register_native_methods` to the export addresses. On the real v2.721.1108 boot it bound **466**
//! libroblox natives (60 classes) + **144** zstd natives (10 classes), advancing the lifecycle from
//! `Application.onCreate` (step 3) through `Activity.createMainActivity` (step 4) into the launcher
//! `ActivitySplash.onCreate` (step 5). [`register_preloaded_natives`] is a small CURATED fallback (the
//! natives first observed unresolved) used only if the reflection pass binds nothing.
//!
//! ## Best-effort / strictly additive
//! Every step is best-effort: a class not yet loadable, a signature drift, or a JNI throw is cleared +
//! logged and never propagates. The pass can only RESOLVE `UnsatisfiedLinkError`s — it never regresses
//! the boot. The two `unsafe` blocks (`JavaVM::from_raw`, `register_native_methods`) carry `// SAFETY:`.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::io::Write;

use jni::objects::{JClass, JObject, JObjectArray, JString};
use jni::strings::JNIString;
use jni::vm::JavaVM;
use jni::{jni_sig, jni_str, Env, NativeMethod};
use jni_sys::JavaVM as RawJavaVM;

use super::jni_mangle::demangle;

/// One pre-loaded native ART reported as "No implementation found": the declaring class (slash-separated
/// binary name), the method, its JNI signature descriptor, and the `Java_*` export whose address (in
/// Eclipse's mapped image) is the implementation.
struct PreloadedNative {
    class: &'static str,
    method: &'static str,
    sig: &'static str,
    symbol: &'static str,
}

/// The natives observed unresolved on the real v2.721.1108 boot. Each is looked up in whichever
/// just-pre-loaded lib exports its `Java_*` symbol (libroblox provides the three `com.roblox.*`;
/// `libzstd-jni` provides `recommendedDInSize`). 2026-06-11.
const PRELOADED_NATIVES: &[PreloadedNative] = &[
    PreloadedNative {
        class: "com/roblox/client/JNIAAssetManagerSetup",
        method: "initNative",
        sig: "(Landroid/content/res/AssetManager;)V",
        symbol: "Java_com_roblox_client_JNIAAssetManagerSetup_initNative",
    },
    PreloadedNative {
        class: "com/roblox/universalapp/logging/JNILoggingProtocol",
        method: "nativeGetTimestamp",
        sig: "()J",
        symbol: "Java_com_roblox_universalapp_logging_JNILoggingProtocol_nativeGetTimestamp",
    },
    PreloadedNative {
        class: "com/roblox/universalapp/logging/JNILoggingProtocol",
        method: "nativeLogEvent",
        sig: "(Ljava/lang/String;J[Ljava/lang/Object;)V",
        symbol: "Java_com_roblox_universalapp_logging_JNILoggingProtocol_nativeLogEvent",
    },
    PreloadedNative {
        class: "com/github/luben/zstd/ZstdInputStreamNoFinalizer",
        method: "recommendedDInSize",
        sig: "()J",
        symbol: "Java_com_github_luben_zstd_ZstdInputStreamNoFinalizer_recommendedDInSize",
    },
];

/// Register any [`PRELOADED_NATIVES`] whose `Java_*` symbol the just-loaded lib exports (looked up via
/// `resolve_export`), binding ART's declared native to that implementation address. Best-effort: a
/// missing class / sig drift / throw is cleared + logged, never fatal (strictly additive).
///
/// `java_vm` MUST be the live process `JavaVM*` from `JNI_CreateJavaVM` (the caller passes
/// `runtime::Vm::as_raw`), valid for this call, and this MUST run on the JNI-attached main thread (the
/// pre-load context). A null `java_vm` is tolerated (logged + skipped). This mirrors the raw-`JavaVM*`
/// convention of the sibling [`super::engine::call_jni_onload`] / `load_app_native_lib`.
// 2026-06-11: `#[allow]` (rather than `unsafe fn`) keeps the lint off the public pre-load API while the
// doc above states the pointer contract — the same raw-`JavaVM*` shape `call_jni_onload` already uses in
// this module. The deref itself is the `unsafe { JavaVM::from_raw }` block below (with its `// SAFETY:`).
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn register_preloaded_natives(
    java_vm: *mut RawJavaVM,
    resolve_export: impl Fn(&str) -> Option<u64>,
    log: &mut impl Write,
) {
    // Only the natives THIS lib actually provides (its image exports the `Java_*` symbol).
    let provided: Vec<(&str, &str, u64, &str)> = PRELOADED_NATIVES
        .iter()
        .filter_map(|n| resolve_export(n.symbol).map(|addr| (n.method, n.sig, addr, n.class)))
        .collect();
    if provided.is_empty() {
        return;
    }
    if java_vm.is_null() {
        let _ = writeln!(log, "engine-load: discovery-gap: null JavaVM — skipped");
        return;
    }

    // SAFETY: `java_vm` is the live process `JavaVM*` from `JNI_CreateJavaVM` (the caller passes
    // `runtime::Vm::as_raw`, non-null — re-checked above). `from_raw` adopts it without taking ownership;
    // the VM outlives this call (it is alive for the whole boot on this main thread).
    let vm = unsafe { JavaVM::from_raw(java_vm) };
    let result: jni::errors::Result<()> = vm.attach_current_thread(|env: &mut Env| {
        for (method, sig, addr, class) in &provided {
            let class_name = JNIString::from(*class);
            let cls = match env.find_class(&class_name) {
                Ok(c) => c,
                Err(_) => {
                    if env.exception_check() {
                        env.exception_clear();
                    }
                    let _ = writeln!(
                        log,
                        "engine-load: discovery-gap: class {class} not loadable yet — skipped {method}"
                    );
                    continue;
                }
            };
            let name_str = JNIString::from(*method);
            let sig_str = JNIString::from(*sig);
            // SAFETY: `addr` is the absolute runtime address of the lib's exported `Java_<class>_<method>`
            // (`resolve_export`, in the PF_X text of the mapped, fully-relocated image); `sig` is that
            // native's declared JNI descriptor (verified against the boot's `No implementation found`
            // line). The two `JNIString`s outlive this `register_native_methods` call.
            let nm =
                unsafe { NativeMethod::from_raw_parts(&name_str, &sig_str, *addr as *mut c_void) };
            // SAFETY: `cls` is the loaded declaring class; `nm` pairs a valid fn address with its real
            // signature. A signature mismatch throws `NoSuchMethodError`, cleared best-effort below.
            match unsafe { env.register_native_methods(&cls, std::slice::from_ref(&nm)) } {
                Ok(()) => {
                    let _ = writeln!(
                        log,
                        "engine-load: discovery-gap: RegisterNatives {class}.{method} → preloaded impl @ {addr:#x} ✓"
                    );
                }
                Err(_) => {
                    if env.exception_check() {
                        env.exception_clear();
                    }
                    let _ = writeln!(
                        log,
                        "engine-load: discovery-gap: RegisterNatives {class}.{method} failed (sig drift / class shape) — skipped"
                    );
                }
            }
        }
        Ok(())
    });
    if let Err(e) = result {
        let _ = writeln!(
            log,
            "engine-load: discovery-gap: JNI attach failed: {e} (skipped — non-fatal)"
        );
    }
}

/// `ACC_NATIVE` — the `java.lang.reflect.Modifier` bit marking a declared method `native`.
const ACC_NATIVE: i32 = 0x0100;

/// The GENERAL discovery-gap fix: RegisterNatives EVERY `Java_*` native the just-loaded lib exports
/// (`exports` = `(symbol_name, absolute_addr)` from [`super::engine::LoadedEngine::java_native_exports`]).
///
/// For each export: [`demangle`] → `(class, method)`; group by class; reflect each class's declared
/// `native` methods for their full JNI signatures (return + parameter types → descriptors); and bind the
/// exported methods (matched by name) to their export addresses. This replicates exactly what ART's lazy
/// `dlsym(handle, "Java_…")` would do — the path broken by Eclipse mmap-pre-loading the lib. Returns the
/// number of natives bound, so the caller can fall back to the curated set if reflection bound nothing.
///
/// Best-effort throughout: a class not loadable, a method with no matching reflected signature, or a JNI
/// throw is cleared + logged + skipped — strictly additive (it never regresses the boot). Local JNI
/// references are scoped per-class and per-method via `with_local_frame`, so reflecting libroblox's ~499
/// natives never overflows the local-reference table. See the module header re: the `#[allow]`.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn register_all_preloaded_natives(
    java_vm: *mut RawJavaVM,
    exports: &[(String, u64)],
    lib_label: &str,
    log: &mut impl Write,
) -> usize {
    if exports.is_empty() || java_vm.is_null() {
        return 0;
    }
    // Demangle + group by declaring class: class binary name → [(method, export addr)].
    let mut by_class: BTreeMap<String, Vec<(String, u64)>> = BTreeMap::new();
    for (sym, addr) in exports {
        if let Some(d) = demangle(sym) {
            by_class.entry(d.class).or_default().push((d.method, *addr));
        }
    }
    if by_class.is_empty() {
        return 0;
    }

    // SAFETY: `java_vm` is the live process `JavaVM*` (caller contract; re-checked non-null above).
    let vm = unsafe { JavaVM::from_raw(java_vm) };
    let mut total_bound = 0usize;
    let result: jni::errors::Result<()> = vm.attach_current_thread(|env: &mut Env| {
        for (class, methods) in &by_class {
            // One local frame per class frees `cls` + the reflected method/type refs before the next.
            let bound: usize = env
                .with_local_frame(16, |env| -> jni::errors::Result<usize> {
                    let class_name = JNIString::from(class.as_str());
                    let cls = match env.find_class(&class_name) {
                        Ok(c) => c,
                        Err(_) => {
                            clear_exception(env);
                            return Ok(0);
                        }
                    };
                    let sigs = match reflect_native_signatures(env, &cls) {
                        Ok(s) => s,
                        Err(_) => {
                            clear_exception(env);
                            return Ok(0);
                        }
                    };
                    Ok(register_class_natives(env, &cls, methods, &sigs))
                })
                .unwrap_or(0);
            total_bound += bound;
        }
        Ok(())
    });
    let _ = writeln!(
        log,
        "engine-load: discovery-gap: {lib_label}: bound {total_bound} Java_* native(s) across {} class(es) via reflection",
        by_class.len()
    );
    if let Err(e) = result {
        let _ = writeln!(
            log,
            "engine-load: discovery-gap: {lib_label}: JNI attach failed: {e} (skipped — non-fatal)"
        );
    }
    total_bound
}

/// Register `methods` (each `(name, export_addr)`) on `cls`, taking each method's JNI signature from the
/// reflected `sigs`. Per-method (best-effort) so one sig mismatch never drops the rest; returns the count
/// bound. The name/sig `JNIString`s outlive their `register_native_methods` call.
fn register_class_natives(
    env: &mut Env,
    cls: &JClass,
    methods: &[(String, u64)],
    sigs: &BTreeMap<String, String>,
) -> usize {
    let mut bound = 0;
    for (method, addr) in methods {
        // No declared `native` of this name (e.g. JNI_OnLoad already bound it, or a stale export).
        let Some(sig) = sigs.get(method) else {
            continue;
        };
        let name_s = JNIString::from(method.as_str());
        let sig_s = JNIString::from(sig.as_str());
        // SAFETY: `addr` is the lib's exported `Java_<class>_<method>` (PF_X text of the relocated
        // image); `sig` is that method's reflected JNI descriptor. The `JNIString`s outlive the call.
        let nm = unsafe { NativeMethod::from_raw_parts(&name_s, &sig_s, *addr as *mut c_void) };
        // SAFETY: `cls` is the loaded declaring class; `nm` pairs a valid fn address with its signature.
        match unsafe { env.register_native_methods(cls, std::slice::from_ref(&nm)) } {
            Ok(()) => bound += 1,
            Err(_) => clear_exception(env),
        }
    }
    bound
}

/// Reflect a class's declared `native` methods into `method_name → JNI signature descriptor`, built from
/// the reflected return + parameter `Class` objects. Each method is reflected inside its own local frame.
fn reflect_native_signatures(
    env: &mut Env,
    cls: &JClass,
) -> jni::errors::Result<BTreeMap<String, String>> {
    let arr_obj = env
        .call_method(
            cls,
            jni_str!("getDeclaredMethods"),
            jni_sig!("()[Ljava/lang/reflect/Method;"),
            &[],
        )?
        .l()?;
    let methods: JObjectArray = env.cast_local::<JObjectArray>(arr_obj)?;
    let n = methods.len(env)?;
    let mut out = BTreeMap::new();
    for i in 0..n {
        let entry: Option<(String, String)> =
            env.with_local_frame(16, |env| -> jni::errors::Result<Option<(String, String)>> {
                let m = methods.get_element(env, i)?;
                let mods = env
                    .call_method(&m, jni_str!("getModifiers"), jni_sig!("()I"), &[])?
                    .i()?;
                if mods & ACC_NATIVE == 0 {
                    return Ok(None);
                }
                let name_obj = env
                    .call_method(
                        &m,
                        jni_str!("getName"),
                        jni_sig!("()Ljava/lang/String;"),
                        &[],
                    )?
                    .l()?;
                let name = jobject_to_string(env, name_obj)?;
                let ret = env
                    .call_method(
                        &m,
                        jni_str!("getReturnType"),
                        jni_sig!("()Ljava/lang/Class;"),
                        &[],
                    )?
                    .l()?;
                let params_obj = env
                    .call_method(
                        &m,
                        jni_str!("getParameterTypes"),
                        jni_sig!("()[Ljava/lang/Class;"),
                        &[],
                    )?
                    .l()?;
                let params: JObjectArray = env.cast_local::<JObjectArray>(params_obj)?;
                let pn = params.len(env)?;
                let mut sig = String::with_capacity(8);
                sig.push('(');
                for j in 0..pn {
                    let p = params.get_element(env, j)?;
                    sig.push_str(&class_descriptor(env, p)?);
                }
                sig.push(')');
                sig.push_str(&class_descriptor(env, ret)?);
                Ok(Some((name, sig)))
            })?;
        if let Some((name, sig)) = entry {
            out.insert(name, sig);
        }
    }
    Ok(out)
}

/// The JNI type descriptor of a `java.lang.Class` instance via `Class.getName()` (`int`→`I`, `void`→`V`,
/// `[B`→`[B`, `java.lang.String`→`Ljava/lang/String;`, `[Ljava.lang.Object;`→`[Ljava/lang/Object;`).
fn class_descriptor(env: &mut Env, class_obj: JObject) -> jni::errors::Result<String> {
    let name_obj = env
        .call_method(
            &class_obj,
            jni_str!("getName"),
            jni_sig!("()Ljava/lang/String;"),
            &[],
        )?
        .l()?;
    let name = jobject_to_string(env, name_obj)?;
    Ok(match name.as_str() {
        "void" => "V".to_string(),
        "boolean" => "Z".to_string(),
        "byte" => "B".to_string(),
        "char" => "C".to_string(),
        "short" => "S".to_string(),
        "int" => "I".to_string(),
        "long" => "J".to_string(),
        "float" => "F".to_string(),
        "double" => "D".to_string(),
        // Arrays: `Class.getName()` is already a `[`-prefixed descriptor (with `.` package separators).
        n if n.starts_with('[') => n.replace('.', "/"),
        // Reference: `Lbinary/name;`.
        n => format!("L{};", n.replace('.', "/")),
    })
}

/// Convert a `java.lang.String` `JObject` to a Rust `String` (modified-UTF-8 → UTF-8).
fn jobject_to_string(env: &mut Env, obj: JObject) -> jni::errors::Result<String> {
    let jstr: JString = env.cast_local::<JString>(obj)?;
    let chars = jstr.mutf8_chars(env)?;
    let owned = String::from(chars);
    Ok(owned)
}

/// Clear a pending JNI exception if one is set (best-effort recovery; never propagates).
fn clear_exception(env: &mut Env) {
    if env.exception_check() {
        env.exception_clear();
    }
}
