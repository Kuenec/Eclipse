use std::sync::OnceLock;

use jni::errors::ThrowRuntimeExAndDefault;
use jni::objects::{JByteArray, JClass, JObjectArray};
use jni::strings::JNIStr;
use jni::{jni_str, Env, EnvUnowned, NativeMethod};

use super::FrameworkError;
use crate::apk::signature::SigningCertificateHistory;

static HISTORY: OnceLock<SigningCertificateHistory> = OnceLock::new();

const SIGNING_CERTIFICATES_CLASS: &JNIStr = jni_str!("android/content/pm/SigningCertificates");
const NATIVE_SIGNING_CERTIFICATE_HISTORY_NAME: &JNIStr =
    jni_str!("native_signingCertificateHistory");
const NATIVE_SIGNING_CERTIFICATE_HISTORY_SIG: &JNIStr = jni_str!("()[[B");

extern "system" fn native_signing_certificate_history<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> JObjectArray<'local, JByteArray<'local>> {
    env.with_env(|env| -> jni::errors::Result<_> {
        let certificates = HISTORY
            .get()
            .expect("register_natives stores the history before binding this native")
            .certificates();
        let array = JObjectArray::<JByteArray>::new(env, certificates.len(), JByteArray::null())?;
        for (index, certificate) in certificates.iter().enumerate() {
            let bytes = env.byte_array_from_slice(certificate)?;
            array.set_element(env, index, &bytes)?;
        }
        Ok(array)
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

pub(super) fn register_natives(
    env: &mut Env,
    history: &SigningCertificateHistory,
) -> Result<(), FrameworkError> {
    if HISTORY.get_or_init(|| history.clone()) != history {
        return Err(FrameworkError::SigningCertificateHistoryConflict);
    }
    let class = env
        .find_class(SIGNING_CERTIFICATES_CLASS)
        .map_err(bridge_class_error)?;
    let methods = [unsafe {
        NativeMethod::from_raw_parts(
            NATIVE_SIGNING_CERTIFICATE_HISTORY_NAME,
            NATIVE_SIGNING_CERTIFICATE_HISTORY_SIG,
            native_signing_certificate_history as *mut std::ffi::c_void,
        )
    }];
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/content/pm/SigningCertificates",
        certificates = history.certificates().len(),
        "registered the host-verified APK signing certificate history"
    );
    Ok(())
}

fn bridge_class_error(error: jni::errors::Error) -> FrameworkError {
    match error {
        jni::errors::Error::NoClassDefFound { .. } => {
            FrameworkError::OverlayPredatesSigningCertificates(error)
        }
        error => FrameworkError::Jni(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_binding_matches_the_overlay_declaration() {
        let java = include_str!(
            "../../tools/framework-overlay/src/android/content/pm/SigningCertificates.java"
        );
        let qualified = SIGNING_CERTIFICATES_CLASS.to_str();
        let (package, class) = qualified
            .rsplit_once('/')
            .expect("the class name is package-qualified");
        assert!(java.starts_with(&format!("package {};", package.replace('/', "."))));
        assert!(java.contains(&format!("public final class {class} {{")));
        assert!(java.contains(&format!(
            "private static native byte[][] {}();",
            NATIVE_SIGNING_CERTIFICATE_HISTORY_NAME.to_str()
        )));
    }

    #[test]
    fn a_missing_bridge_class_asks_for_an_overlay_rebuild() {
        let missing = bridge_class_error(jni::errors::Error::NoClassDefFound {
            requested: SIGNING_CERTIFICATES_CLASS.to_str().to_string(),
            cause: None,
        });
        assert!(
            matches!(
                missing,
                FrameworkError::OverlayPredatesSigningCertificates(_)
            ),
            "{missing:?}"
        );
        let message = missing.to_string();
        assert!(
            message.contains("tools/framework-overlay/patch-framework.sh"),
            "{message}"
        );
        assert!(
            message.contains("android/content/pm/SigningCertificates"),
            "{message}"
        );

        let other = bridge_class_error(jni::errors::Error::ClassFormatError);
        assert!(matches!(other, FrameworkError::Jni(_)), "{other:?}");
    }
}
