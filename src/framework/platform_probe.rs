use std::fmt;
use std::panic::AssertUnwindSafe;

use jni::objects::JObject;
use jni::vm::JavaVM;
use jni::{jni_sig, jni_str, Env, JValue};

use super::{
    captures, checked, jstring_object_to_string, register_environment_natives, FrameworkError,
    ENVIRONMENT_CLASS,
};
use crate::runtime::Vm;

pub const CAPTURE_PROBE_FILE: &str = "eclipse-platform-probe.png";
pub const CAPTURE_PROBE_BYTES: [u8; 4] = [0x89, b'P', b'N', b'G'];

pub struct PlatformProbeReport {
    pub pictures: String,
    pub movies: String,
    pub temporary: String,
}

impl fmt::Display for PlatformProbeReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Pictures directory: {}", self.pictures)?;
        writeln!(
            f,
            "wrote {} bytes to {CAPTURE_PROBE_FILE} through FileOutputStream",
            CAPTURE_PROBE_BYTES.len()
        )?;
        writeln!(f, "Movies directory: {}", self.movies)?;
        write!(f, "Temporary directory: {}", self.temporary)
    }
}

pub fn run(vm: &Vm) -> Result<PlatformProbeReport, FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| probe(env))) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

fn probe(env: &mut Env) -> Result<PlatformProbeReport, FrameworkError> {
    register_environment_natives(env)?;
    captures::register_natives(env)?;

    let pictures = public_directory(env, "Pictures")?;
    write_capture(env, &pictures)?;
    let pictures = path_of(env, pictures)?;
    let movies = public_directory(env, "Movies")?;
    let movies = path_of(env, movies)?;
    let temporary = system_property(env, "java.io.tmpdir")?;
    Ok(PlatformProbeReport {
        pictures,
        movies,
        temporary,
    })
}

fn system_property(env: &mut Env, key: &str) -> Result<String, FrameworkError> {
    let key = env.new_string(key)?;
    let value = checked(env, "System.getProperty", |env| {
        env.call_static_method(
            jni_str!("java/lang/System"),
            jni_str!("getProperty"),
            jni_sig!("(Ljava/lang/String;)Ljava/lang/String;"),
            &[JValue::Object(&key)],
        )?
        .l()
    })?;
    jstring_object_to_string(env, value).map_err(FrameworkError::Jni)
}

fn public_directory<'local>(
    env: &mut Env<'local>,
    kind: &str,
) -> Result<JObject<'local>, FrameworkError> {
    let kind = env.new_string(kind)?;
    checked(env, "Environment public directory", |env| {
        env.call_static_method(
            ENVIRONMENT_CLASS,
            jni_str!("getExternalStoragePublicDirectory"),
            jni_sig!("(Ljava/lang/String;)Ljava/io/File;"),
            &[JValue::Object(&kind)],
        )?
        .l()
    })
}

fn write_capture(env: &mut Env, directory: &JObject) -> Result<(), FrameworkError> {
    let name = env.new_string(CAPTURE_PROBE_FILE)?;
    let file = checked(env, "new File(directory, name)", |env| {
        env.new_object(
            jni_str!("java/io/File"),
            jni_sig!("(Ljava/io/File;Ljava/lang/String;)V"),
            &[JValue::Object(directory), JValue::Object(&name)],
        )
    })?;
    let stream = checked(env, "new FileOutputStream(file)", |env| {
        env.new_object(
            jni_str!("java/io/FileOutputStream"),
            jni_sig!("(Ljava/io/File;)V"),
            &[JValue::Object(&file)],
        )
    })?;
    let bytes = env.byte_array_from_slice(&CAPTURE_PROBE_BYTES)?;
    checked(env, "FileOutputStream.write", |env| {
        env.call_method(
            &stream,
            jni_str!("write"),
            jni_sig!("([B)V"),
            &[JValue::Object(&bytes)],
        )?
        .v()
    })?;
    checked(env, "FileOutputStream.close", |env| {
        env.call_method(&stream, jni_str!("close"), jni_sig!("()V"), &[])?
            .v()
    })
}

fn path_of(env: &mut Env, file: JObject) -> Result<String, FrameworkError> {
    let path = checked(env, "File.getPath", |env| {
        env.call_method(
            &file,
            jni_str!("getPath"),
            jni_sig!("()Ljava/lang/String;"),
            &[],
        )?
        .l()
    })?;
    jstring_object_to_string(env, path).map_err(FrameworkError::Jni)
}
