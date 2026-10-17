use std::ffi::c_void;

use jni::errors::ThrowRuntimeExAndDefault;
use jni::objects::{JByteArray, JClass, JIntArray, JObject, JObjectArray};
use jni::signature::MethodSignature;
use jni::strings::JNIStr;
use jni::sys::jint;
use jni::{jni_sig, jni_str, Env, EnvUnowned, JValue, NativeMethod};

use super::{checked, FrameworkError};

pub(super) const DEX_FILE_CLASS: &JNIStr = jni_str!("dalvik/system/DexFile");
pub(super) const BYTE_BUFFER_CLASS: &JNIStr = jni_str!("java/nio/ByteBuffer");
const BYTE_ARRAY_CLASS: &JNIStr = jni_str!("[B");

const CREATE_COOKIE_WITH_ARRAY_NAME: &JNIStr = jni_str!("createCookieWithArray");
const CREATE_COOKIE_WITH_ARRAY_SIG: &JNIStr = jni_str!("([BII)Ljava/lang/Object;");
const CREATE_COOKIE_WITH_DIRECT_BUFFER_NAME: &JNIStr = jni_str!("createCookieWithDirectBuffer");
const CREATE_COOKIE_WITH_DIRECT_BUFFER_SIG: &JNIStr =
    jni_str!("(Ljava/nio/ByteBuffer;II)Ljava/lang/Object;");

const OPEN_IN_MEMORY_DEX_FILES_NAME: &JNIStr = jni_str!("openInMemoryDexFilesNative");
const OPEN_IN_MEMORY_DEX_FILES_SIG: MethodSignature<'static, 'static> = jni_sig!(
    "([Ljava/nio/ByteBuffer;[[B[I[ILjava/lang/ClassLoader;[Ldalvik/system/DexPathList$Element;)Ljava/lang/Object;"
);

pub(super) fn register_natives(env: &mut Env) -> Result<(), FrameworkError> {
    checked(
        env,
        "RegisterNatives dalvik/system/DexFile createCookieWith*",
        |env| {
            let class = env.find_class(DEX_FILE_CLASS)?;
            let methods = [
                unsafe {
                    NativeMethod::from_raw_parts(
                        CREATE_COOKIE_WITH_ARRAY_NAME,
                        CREATE_COOKIE_WITH_ARRAY_SIG,
                        dex_file_create_cookie_with_array as *mut c_void,
                    )
                },
                unsafe {
                    NativeMethod::from_raw_parts(
                        CREATE_COOKIE_WITH_DIRECT_BUFFER_NAME,
                        CREATE_COOKIE_WITH_DIRECT_BUFFER_SIG,
                        dex_file_create_cookie_with_direct_buffer as *mut c_void,
                    )
                },
            ];
            unsafe { env.register_native_methods(&class, &methods) }
        },
    )
}

extern "system" fn dex_file_create_cookie_with_array<'local>(
    mut env: EnvUnowned<'local>,
    class: JClass<'local>,
    dex: JByteArray<'local>,
    start: jint,
    end: jint,
) -> JObject<'local> {
    env.with_env(|env| -> jni::errors::Result<JObject<'local>> {
        let buffers = env.new_object_array(1, BYTE_BUFFER_CLASS, JObject::null())?;
        let arrays = env.new_object_array(1, BYTE_ARRAY_CLASS, &dex)?;
        open_in_memory_dex_file(env, &class, &buffers, &arrays, start, end)
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

extern "system" fn dex_file_create_cookie_with_direct_buffer<'local>(
    mut env: EnvUnowned<'local>,
    class: JClass<'local>,
    dex: JObject<'local>,
    start: jint,
    end: jint,
) -> JObject<'local> {
    env.with_env(|env| -> jni::errors::Result<JObject<'local>> {
        let buffers = env.new_object_array(1, BYTE_BUFFER_CLASS, &dex)?;
        let arrays = env.new_object_array(1, BYTE_ARRAY_CLASS, JObject::null())?;
        open_in_memory_dex_file(env, &class, &buffers, &arrays, start, end)
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

fn open_in_memory_dex_file<'local>(
    env: &mut Env<'local>,
    dex_file_class: &JClass,
    buffers: &JObjectArray,
    arrays: &JObjectArray,
    start: jint,
    end: jint,
) -> jni::errors::Result<JObject<'local>> {
    let starts = JIntArray::new(env, 1)?;
    starts.set_region(env, 0, &[start])?;
    let ends = JIntArray::new(env, 1)?;
    ends.set_region(env, 0, &[end])?;
    let no_class_loader = JObject::null();
    let no_dex_elements = JObject::null();
    env.call_static_method(
        dex_file_class,
        OPEN_IN_MEMORY_DEX_FILES_NAME,
        OPEN_IN_MEMORY_DEX_FILES_SIG,
        &[
            JValue::Object(buffers),
            JValue::Object(arrays),
            JValue::Object(&starts),
            JValue::Object(&ends),
            JValue::Object(&no_class_loader),
            JValue::Object(&no_dex_elements),
        ],
    )?
    .l()
}
