use std::fmt;
use std::panic::AssertUnwindSafe;

use jni::objects::JObject;
use jni::vm::JavaVM;
use jni::{jni_sig, jni_str, Env, JValue};

use super::engine_input::{probe_second_finger, TouchEventReadback};
use super::{
    checked, external_intents, jstring_object_to_string, keep_screen_on, notifications,
    register_framework_natives, take_pending_host_clipboard_text, FrameworkError, ACTIVITY_CLASS,
    CONTEXT_CLASS, ENVIRONMENT_CLASS, WINDOW_CLASS,
};
use crate::apk::signature::SigningCertificateHistory;
use crate::runtime::Vm;

pub const CAPTURE_PROBE_FILE: &str = "eclipse-platform-probe.png";
pub const CAPTURE_PROBE_BYTES: [u8; 4] = [0x89, b'P', b'N', b'G'];

const NOTIFICATION_PROBE_ID: i32 = 7;
const NOTIFICATION_PROBE_TITLE: &str = "Title";
const NOTIFICATION_PROBE_BODY: &str = "Body";
const NO_INTENT_TYPE: i32 = -1;

const OPENED_LINK: &str = "https://example.org/x";
const REFUSED_LINK: &str = "roblox://placeId=1";
const SHARED_TEXT: &str = "hi https://www.roblox.com/share?code=X";
const NO_SHARED_FILE: i32 = -1;

enum NativeOutcome {
    Returned,
    Threw(String),
}

impl fmt::Display for NativeOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Returned => f.write_str("returned"),
            Self::Threw(exception) => write!(f, "threw {exception}"),
        }
    }
}

#[derive(Clone, Copy)]
enum WindowFlagCall {
    Add(i32),
    Clear(i32),
    Set(i32, i32),
}

impl fmt::Display for WindowFlagCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Add(flags) => write!(f, "addFlags({flags:#x})"),
            Self::Clear(flags) => write!(f, "clearFlags({flags:#x})"),
            Self::Set(flags, mask) => write!(f, "setFlags({flags:#x}, {mask:#x})"),
        }
    }
}

const FLAG_KEEP_SCREEN_ON: i32 = 0x80;
const FLAG_FULLSCREEN: i32 = 0x400;

const WINDOW_FLAG_CALLS: [WindowFlagCall; 6] = [
    WindowFlagCall::Add(FLAG_KEEP_SCREEN_ON),
    WindowFlagCall::Add(FLAG_FULLSCREEN),
    WindowFlagCall::Clear(FLAG_KEEP_SCREEN_ON),
    WindowFlagCall::Set(FLAG_KEEP_SCREEN_ON, FLAG_KEEP_SCREEN_ON),
    WindowFlagCall::Set(0, FLAG_KEEP_SCREEN_ON),
    WindowFlagCall::Set(0, FLAG_FULLSCREEN),
];

pub struct PlatformProbeReport {
    pub pictures: String,
    pub movies: String,
    pub temporary: String,
    keep_screen_on: Vec<(WindowFlagCall, bool)>,
    notification_builder: i64,
    opened_link: NativeOutcome,
    refused_link: NativeOutcome,
    shared_text: NativeOutcome,
    host_clipboard: Option<String>,
    second_finger: TouchEventReadback,
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
        writeln!(f, "Temporary directory: {}", self.temporary)?;
        for (call, keep_screen_on) in &self.keep_screen_on {
            let screen = if *keep_screen_on {
                "keeps the screen on"
            } else {
                "lets the screen sleep"
            };
            writeln!(f, "Window.{call} {screen}")?;
        }
        writeln!(
            f,
            "NotificationManager natives returned without an exception; nativeInitBuilder \
             returned {}",
            self.notification_builder
        )?;
        writeln!(
            f,
            "Activity.nativeOpenURI({OPENED_LINK:?}) {}",
            self.opened_link
        )?;
        writeln!(
            f,
            "Activity.nativeOpenURI({REFUSED_LINK:?}) {}",
            self.refused_link
        )?;
        writeln!(
            f,
            "Context.nativeShareFile({SHARED_TEXT:?}, {NO_SHARED_FILE}) {}",
            self.shared_text
        )?;
        writeln!(f, "{}", self.second_finger)?;
        match &self.host_clipboard {
            Some(text) => write!(f, "host clipboard slot holds {text:?}"),
            None => f.write_str("host clipboard slot is empty"),
        }
    }
}

pub fn run(
    vm: &Vm,
    apk_path: &str,
    signing_certificate_history: &SigningCertificateHistory,
) -> Result<PlatformProbeReport, FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| {
            probe(env, apk_path, signing_certificate_history)
        })) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

fn probe(
    env: &mut Env,
    apk_path: &str,
    signing_certificate_history: &SigningCertificateHistory,
) -> Result<PlatformProbeReport, FrameworkError> {
    register_framework_natives(env, apk_path, signing_certificate_history)?;

    let pictures = public_directory(env, "Pictures")?;
    write_capture(env, &pictures)?;
    let pictures = path_of(env, pictures)?;
    let movies = public_directory(env, "Movies")?;
    let movies = path_of(env, movies)?;
    let temporary = system_property(env, "java.io.tmpdir")?;
    let keep_screen_on = window_flag_calls(env)?;
    let notification_builder = client_notifications(env)?;
    let opened_link = open_uri(env, OPENED_LINK)?;
    let refused_link = open_uri(env, REFUSED_LINK)?;
    let shared_text = share_text(env, SHARED_TEXT)?;
    let second_finger = probe_second_finger(env)?;
    Ok(PlatformProbeReport {
        pictures,
        movies,
        temporary,
        keep_screen_on,
        notification_builder,
        opened_link,
        refused_link,
        shared_text,
        host_clipboard: take_pending_host_clipboard_text(),
        second_finger,
    })
}

fn open_uri(env: &mut Env, link: &str) -> Result<NativeOutcome, FrameworkError> {
    let link = env.new_string(link)?;
    let called = env.call_static_method(
        ACTIVITY_CLASS,
        external_intents::NATIVE_OPEN_URI_NAME,
        external_intents::NATIVE_OPEN_URI_SIG,
        &[JValue::Object(&link)],
    );
    outcome(env, called)
}

fn share_text(env: &mut Env, text: &str) -> Result<NativeOutcome, FrameworkError> {
    let text = env.new_string(text)?;
    let called = env.call_static_method(
        CONTEXT_CLASS,
        external_intents::NATIVE_SHARE_FILE_NAME,
        external_intents::NATIVE_SHARE_FILE_SIG,
        &[JValue::Object(&text), JValue::Int(NO_SHARED_FILE)],
    );
    outcome(env, called)
}

fn outcome<T>(
    env: &mut Env,
    called: Result<T, jni::errors::Error>,
) -> Result<NativeOutcome, FrameworkError> {
    let Err(error) = called else {
        return Ok(NativeOutcome::Returned);
    };
    match env.exception_catch() {
        Err(jni::errors::Error::CaughtJavaException { name, .. }) => Ok(NativeOutcome::Threw(name)),
        Ok(()) | Err(_) => Err(FrameworkError::Jni(error)),
    }
}

fn client_notifications(env: &mut Env) -> Result<i64, FrameworkError> {
    let manager = checked(env, "AllocObject android.app.NotificationManager", |env| {
        let class = env.find_class(notifications::NOTIFICATION_MANAGER_CLASS)?;
        env.alloc_object(&class)
    })?;
    let title = env.new_string(NOTIFICATION_PROBE_TITLE)?;
    let body = env.new_string(NOTIFICATION_PROBE_BODY)?;
    let none = JObject::null();
    let builder = checked(env, "NotificationManager.nativeInitBuilder", |env| {
        env.call_method(
            &manager,
            jni_str!("nativeInitBuilder"),
            jni_sig!("()J"),
            &[],
        )?
        .j()
    })?;
    checked(env, "NotificationManager.nativeAddAction", |env| {
        env.call_method(
            &manager,
            jni_str!("nativeAddAction"),
            jni_sig!("(JLjava/lang/String;ILandroid/content/Intent;)V"),
            &[
                JValue::Long(builder),
                JValue::Object(&title),
                JValue::Int(NO_INTENT_TYPE),
                JValue::Object(&none),
            ],
        )?
        .v()
    })?;
    for ongoing in [false, true] {
        checked(env, "NotificationManager.nativeShowNotification", |env| {
            env.call_method(
                &manager,
                jni_str!("nativeShowNotification"),
                jni_sig!(
                    "(JILjava/lang/String;Ljava/lang/String;Ljava/lang/String;ZILandroid/content/Intent;)V"
                ),
                &[
                    JValue::Long(builder),
                    JValue::Int(NOTIFICATION_PROBE_ID),
                    JValue::Object(&title),
                    JValue::Object(&body),
                    JValue::Object(&none),
                    JValue::Bool(ongoing),
                    JValue::Int(NO_INTENT_TYPE),
                    JValue::Object(&none),
                ],
            )?
            .v()
        })?;
    }
    checked(env, "NotificationManager.nativeCancel", |env| {
        env.call_method(
            &manager,
            jni_str!("nativeCancel"),
            jni_sig!("(I)V"),
            &[JValue::Int(NOTIFICATION_PROBE_ID)],
        )?
        .v()
    })?;
    let package = env.new_string("com.roblox.client")?;
    let identity = env.new_string("Roblox")?;
    checked(env, "NotificationManager.nativeShowMPRIS", |env| {
        env.call_method(
            &manager,
            jni_str!("nativeShowMPRIS"),
            jni_sig!("(Ljava/lang/String;Ljava/lang/String;)V"),
            &[JValue::Object(&package), JValue::Object(&identity)],
        )?
        .v()
    })?;
    checked(env, "NotificationManager.nativeCancelMPRIS", |env| {
        env.call_method(
            &manager,
            jni_str!("nativeCancelMPRIS"),
            jni_sig!("()V"),
            &[],
        )?
        .v()
    })?;
    Ok(builder)
}

fn window_flag_calls(env: &mut Env) -> Result<Vec<(WindowFlagCall, bool)>, FrameworkError> {
    let window = checked(env, "AllocObject android.view.Window", |env| {
        let class = env.find_class(WINDOW_CLASS)?;
        env.alloc_object(&class)
    })?;
    let mut observed = Vec::with_capacity(WINDOW_FLAG_CALLS.len());
    for call in WINDOW_FLAG_CALLS {
        let what = format!("Window.{call}");
        checked(env, &what, |env| {
            match call {
                WindowFlagCall::Add(flags) => env.call_method(
                    &window,
                    jni_str!("addFlags"),
                    jni_sig!("(I)V"),
                    &[JValue::Int(flags)],
                ),
                WindowFlagCall::Clear(flags) => env.call_method(
                    &window,
                    jni_str!("clearFlags"),
                    jni_sig!("(I)V"),
                    &[JValue::Int(flags)],
                ),
                WindowFlagCall::Set(flags, mask) => env.call_method(
                    &window,
                    jni_str!("setFlags"),
                    jni_sig!("(II)V"),
                    &[JValue::Int(flags), JValue::Int(mask)],
                ),
            }?
            .v()
        })?;
        observed.push((call, keep_screen_on::client_keeps_screen_on()));
    }
    Ok(observed)
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
