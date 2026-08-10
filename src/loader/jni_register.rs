use std::collections::BTreeMap;
use std::ffi::c_void;
use std::io::Write;

use jni::objects::{JClass, JObject, JObjectArray, JString};
use jni::strings::JNIString;
use jni::vm::JavaVM;
use jni::{jni_sig, jni_str, Env, NativeMethod};
use jni_sys::JavaVM as RawJavaVM;

use super::jni_mangle::demangle;

struct PreloadedNative {
    class: &'static str,
    method: &'static str,
    sig: &'static str,
    symbol: &'static str,
}

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

#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn register_preloaded_natives(
    java_vm: *mut RawJavaVM,
    resolve_export: impl Fn(&str) -> Option<u64>,
    log: &mut impl Write,
) {
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

            let nm =
                unsafe { NativeMethod::from_raw_parts(&name_str, &sig_str, *addr as *mut c_void) };

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

const ACC_NATIVE: i32 = 0x0100;

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

    let mut by_class: BTreeMap<String, Vec<(String, u64)>> = BTreeMap::new();
    for (sym, addr) in exports {
        if let Some(d) = demangle(sym) {
            by_class.entry(d.class).or_default().push((d.method, *addr));
        }
    }
    if by_class.is_empty() {
        return 0;
    }

    let vm = unsafe { JavaVM::from_raw(java_vm) };
    let mut total_bound = 0usize;
    let result: jni::errors::Result<()> = vm.attach_current_thread(|env: &mut Env| {
        for (class, methods) in &by_class {
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

fn register_class_natives(
    env: &mut Env,
    cls: &JClass,
    methods: &[(String, u64)],
    sigs: &BTreeMap<String, String>,
) -> usize {
    let mut bound = 0;
    for (method, addr) in methods {
        let Some(sig) = sigs.get(method) else {
            continue;
        };
        let name_s = JNIString::from(method.as_str());
        let sig_s = JNIString::from(sig.as_str());

        let nm = unsafe { NativeMethod::from_raw_parts(&name_s, &sig_s, *addr as *mut c_void) };

        match unsafe { env.register_native_methods(cls, std::slice::from_ref(&nm)) } {
            Ok(()) => bound += 1,
            Err(_) => clear_exception(env),
        }
    }
    bound
}

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

        n if n.starts_with('[') => n.replace('.', "/"),

        n => format!("L{};", n.replace('.', "/")),
    })
}

fn jobject_to_string(env: &mut Env, obj: JObject) -> jni::errors::Result<String> {
    let jstr: JString = env.cast_local::<JString>(obj)?;
    let chars = jstr.mutf8_chars(env)?;
    let owned = String::from(chars);
    Ok(owned)
}

fn clear_exception(env: &mut Env) {
    if env.exception_check() {
        env.exception_clear();
    }
}
