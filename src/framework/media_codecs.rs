use std::borrow::Cow;

use jni::errors::ThrowRuntimeExAndDefault;
use jni::objects::{JClass, JObjectArray, JString};
use jni::strings::JNIStr;
use jni::{jni_str, Env, EnvUnowned, NativeMethod};

use super::FrameworkError;
use crate::loader::mediacodec::avcodec::{self, Codec};

const MEDIA_CODEC_LIST_CLASS: &JNIStr = jni_str!("android/media/MediaCodecList");
const NATIVE_DECODERS_NAME: &JNIStr = jni_str!("native_decoders");
const NATIVE_DECODERS_SIG: &JNIStr = jni_str!("()[Ljava/lang/String;");

fn decoder_pairs(decoders: &[Codec]) -> impl Iterator<Item = [Cow<'static, str>; 2]> + '_ {
    decoders.iter().map(|codec| {
        [
            codec.decoder_name().to_string_lossy(),
            Cow::Borrowed(codec.mime()),
        ]
    })
}

extern "system" fn native_decoders<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> JObjectArray<'local, JString<'local>> {
    env.with_env(|env| -> jni::errors::Result<_> {
        let decoders = avcodec::available_decoders();
        let array = JObjectArray::<JString>::new(env, decoders.len() * 2, JString::null())?;
        for (index, text) in decoder_pairs(&decoders).flatten().enumerate() {
            let text = env.new_string(text)?;
            array.set_element(env, index, &text)?;
        }
        tracing::debug!(target: "mediandk", ?decoders, "MediaCodecList.native_decoders");
        Ok(array)
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

pub(super) fn register_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(MEDIA_CODEC_LIST_CLASS)?;
    let methods = [unsafe {
        NativeMethod::from_raw_parts(
            NATIVE_DECODERS_NAME,
            NATIVE_DECODERS_SIG,
            native_decoders as *mut std::ffi::c_void,
        )
    }];
    unsafe { env.register_native_methods(&class, &methods) }.map_err(registration_error)
}

fn registration_error(error: jni::errors::Error) -> FrameworkError {
    match error {
        jni::errors::Error::NoSuchMethod(_) => FrameworkError::OverlayPredatesMediaCodecList(error),
        error => FrameworkError::Jni(error),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::{c_char, c_int, c_void, CString};

    use super::*;
    use crate::framework::fake_jvm;
    use crate::loader::native_provider::EclipseNativeProvider;
    use crate::loader::resolve::SymbolProvider;

    type CreateFn = unsafe extern "C" fn(*const c_char) -> *mut c_void;
    type DeleteFn = unsafe extern "C" fn(*mut c_void) -> c_int;

    fn native(provider: &EclipseNativeProvider, name: &str) -> usize {
        let addr = provider
            .resolve(name)
            .unwrap_or_else(|| panic!("{name} must be an Eclipse native"))
            .addr;
        usize::try_from(addr).expect("a native address fits a pointer")
    }

    #[test]
    fn the_native_matches_the_overlay_declaration() {
        let java =
            include_str!("../../tools/framework-overlay/src/android/media/MediaCodecList.java");
        let qualified = MEDIA_CODEC_LIST_CLASS.to_str();
        let (package, class) = qualified
            .rsplit_once('/')
            .expect("the class name is package-qualified");
        assert!(java.starts_with(&format!("package {};", package.replace('/', "."))));
        assert!(java.contains(&format!("public final class {class} {{")));
        assert!(java.contains(&format!(
            "private static native String[] {}();",
            NATIVE_DECODERS_NAME.to_str()
        )));
        assert_eq!(NATIVE_DECODERS_SIG.to_str(), "()[Ljava/lang/String;");
    }

    #[test]
    fn the_decoder_table_native_is_bound() {
        fake_jvm::with_env(register_natives).expect("register the codec list native");
        assert!(fake_jvm::registered_native(
            &MEDIA_CODEC_LIST_CLASS.to_str(),
            &NATIVE_DECODERS_NAME.to_str(),
            &NATIVE_DECODERS_SIG.to_str()
        )
        .is_some());
    }

    #[test]
    fn the_native_returns_each_available_decoder_name_then_its_mime() {
        let decoders = avcodec::available_decoders();
        let expected: Vec<Option<String>> = decoder_pairs(&decoders)
            .flatten()
            .map(|text| Some(text.into_owned()))
            .collect();
        assert_eq!(expected.len(), decoders.len() * 2);
        let array = native_decoders(fake_jvm::native_env(), JClass::null());
        assert_eq!(fake_jvm::take_exception(), None);
        assert_eq!(fake_jvm::array_texts(&array), expected);
    }

    #[test]
    fn a_codec_list_without_the_native_asks_for_an_overlay_rebuild() {
        let missing = registration_error(jni::errors::Error::NoSuchMethod(
            "no native method \"native_decoders\"".to_owned(),
        ));
        assert!(
            matches!(missing, FrameworkError::OverlayPredatesMediaCodecList(_)),
            "{missing:?}"
        );
        let message = missing.to_string();
        assert!(
            message.contains("tools/framework-overlay/patch-framework.sh"),
            "{message}"
        );

        let other = registration_error(jni::errors::Error::ClassFormatError);
        assert!(matches!(other, FrameworkError::Jni(_)), "{other:?}");
    }

    #[test]
    fn every_listed_decoder_is_one_create_decoder_by_type_accepts() {
        for [name, mime] in decoder_pairs(&Codec::ALL) {
            let codec = Codec::for_mime(mime.as_bytes())
                .unwrap_or_else(|| panic!("createDecoderByType has no row for {mime}"));
            assert_eq!(codec.decoder_name().to_string_lossy(), name);
        }

        let decoders = avcodec::available_decoders();
        if decoders.is_empty() {
            eprintln!(
                "every_listed_decoder_is_one_create_decoder_by_type_accepts: no FFmpeg \
                 decoders; skipping the live check"
            );
            return;
        }
        let provider = EclipseNativeProvider::with_bionic_natives();
        let create = native(&provider, "AMediaCodec_createDecoderByType");
        let delete = native(&provider, "AMediaCodec_delete");
        let create: CreateFn = unsafe { std::mem::transmute::<usize, CreateFn>(create) };
        let delete: DeleteFn = unsafe { std::mem::transmute::<usize, DeleteFn>(delete) };
        for [name, mime] in decoder_pairs(&decoders) {
            let mime = CString::new(mime.as_ref()).expect("a MIME type has no NUL");
            let codec = unsafe { create(mime.as_ptr()) };
            assert!(
                !codec.is_null(),
                "createDecoderByType({mime:?}) creates the listed {name} decoder"
            );
            assert_eq!(unsafe { delete(codec) }, 0, "delete the {name} decoder");
        }
    }
}
