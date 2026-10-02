use std::sync::OnceLock;

use jni::errors::ThrowRuntimeExAndDefault;
use jni::objects::{JClass, JString};
use jni::refs::Global;
use jni::strings::JNIStr;
use jni::{jni_sig, jni_str, Env, EnvUnowned, JValue, NativeMethod};

use super::FrameworkError;

const PRELOADED_LIBRARY_CLASS: &JNIStr = jni_str!("android/os/PreloadedLibrary");
const INITIALIZE_NAME: &JNIStr = jni_str!("initialize");
const INITIALIZE_SIG: &JNIStr = jni_str!("(Ljava/lang/String;)Ljava/lang/String;");

static CLASS: OnceLock<Global<JClass<'static>>> = OnceLock::new();

extern "system" fn initialize<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    name: JString<'local>,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        let name = name.try_to_string(env)?;
        let java_vm = env.get_java_vm()?;
        match crate::loader::engine::initialize_preloaded_lib(
            &name,
            &java_vm,
            &mut std::io::stdout(),
        ) {
            Ok(()) => Ok(JString::default()),
            Err(error) => env.new_string(error),
        }
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

pub(super) fn register_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env
        .find_class(PRELOADED_LIBRARY_CLASS)
        .map_err(|error| match error {
            jni::errors::Error::NoClassDefFound { .. } => {
                FrameworkError::OverlayPredatesPreloadedLibraries(error)
            }
            error => FrameworkError::Jni(error),
        })?;
    let methods = [unsafe {
        NativeMethod::from_raw_parts(
            INITIALIZE_NAME,
            INITIALIZE_SIG,
            initialize as *mut std::ffi::c_void,
        )
    }];
    unsafe { env.register_native_methods(&class, &methods) }?;
    if CLASS.get().is_none() {
        let _ = CLASS.set(env.new_global_ref(&class)?);
    }
    tracing::info!(
        class = "android/os/PreloadedLibrary",
        "registered the app-class-loader frame for preloaded libraries' JNI_OnLoad"
    );
    Ok(())
}

pub(super) fn initialize_in_app_class_loader<'local>(
    env: &mut Env<'local>,
    name: &str,
) -> jni::errors::Result<Option<JString<'local>>> {
    let Some(class) = CLASS.get() else {
        return env
            .new_string(format!(
                "Eclipse: cannot initialize \"{name}\": android.os.PreloadedLibrary is not registered"
            ))
            .map(Some);
    };
    let name = env.new_string(name)?;
    let error = env
        .call_static_method(
            class,
            INITIALIZE_NAME,
            jni_sig!("(Ljava/lang/String;)Ljava/lang/String;"),
            &[JValue::Object(&name)],
        )?
        .l()?;
    if error.is_null() {
        return Ok(None);
    }
    env.cast_local::<JString>(error).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_native_matches_the_overlay_declaration() {
        let java =
            include_str!("../../tools/framework-overlay/src/android/os/PreloadedLibrary.java");
        assert!(java.contains("package android.os;"));
        assert!(java.contains("public static native String initialize(String name);"));
        assert_eq!(
            PRELOADED_LIBRARY_CLASS.to_str(),
            "android/os/PreloadedLibrary"
        );
        assert_eq!(INITIALIZE_NAME.to_str(), "initialize");
        assert_eq!(
            INITIALIZE_SIG.to_str(),
            "(Ljava/lang/String;)Ljava/lang/String;"
        );
    }
}
