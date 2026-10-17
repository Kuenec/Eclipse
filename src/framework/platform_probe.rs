use std::ffi::c_void;
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::{Mutex, PoisonError};

use jni::errors::LogErrorAndDefault;
use jni::objects::JObject;
use jni::strings::JNIStr;
use jni::sys::{jboolean, jint, jlong};
use jni::vm::JavaVM;
use jni::{jni_sig, jni_str, Env, EnvUnowned, JValue, NativeMethod};
use ring::digest::{digest, SHA1_FOR_LEGACY_USE_ONLY};

use super::appsflyer_statics::{self, AppsFlyerStatics};
use super::engine_input::{
    probe_point_move, probe_second_finger, PointEventReadback, TouchEventReadback,
};
use super::in_memory_dex::{BYTE_BUFFER_CLASS, DEX_FILE_CLASS};
use super::{
    checked, external_intents, jstring_object_to_string, keep_screen_on, notifications,
    register_framework_natives, take_pending_host_clipboard_text, FrameworkError, ACTIVITY_CLASS,
    ATL_LOADED_APP_CLASS, CONTEXT_CLASS, ENVIRONMENT_CLASS, WINDOW_CLASS,
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

const CLASS_LOADER_CLASS: &JNIStr = jni_str!("java/lang/ClassLoader");
const IN_MEMORY_DEX_CLASS_LOADER_CLASS: &JNIStr = jni_str!("dalvik/system/InMemoryDexClassLoader");
const PROBE_DEX_CLASS: &str = "eclipse.InMemoryDexProbe";
const PROBE_DEX_PADDING: usize = 3;
const OBJECT_DESCRIPTOR: &str = "Ljava/lang/Object;";
const DEX_MAGIC: &[u8; 8] = b"dex\n035\0";
const DEX_CHECKSUM_OFFSET: usize = 8;
const DEX_SIGNATURE_OFFSET: usize = 12;
const DEX_FILE_SIZE_OFFSET: usize = 32;
const DEX_HEADER_SIZE: u32 = 0x70;
const DEX_ENDIAN_CONSTANT: u32 = 0x1234_5678;
const DEX_NO_INDEX: u32 = u32::MAX;
const DEX_ACC_PUBLIC: u32 = 0x1;
const DEX_CLASS_DEF_WORDS: u32 = 8;
const DEX_MAP_HEADER_ITEM: u32 = 0x0000;
const DEX_MAP_STRING_ID_ITEM: u32 = 0x0001;
const DEX_MAP_TYPE_ID_ITEM: u32 = 0x0002;
const DEX_MAP_CLASS_DEF_ITEM: u32 = 0x0006;
const DEX_MAP_MAP_LIST: u32 = 0x1000;
const DEX_MAP_STRING_DATA_ITEM: u32 = 0x2002;
const ADLER_MODULUS: u32 = 65_521;

const CONTEXT_UTILS_CLASS: &JNIStr = jni_str!("org/webrtc/ContextUtils");
const WEBRTC_AUDIO_MANAGER_CLASS: &JNIStr = jni_str!("org/webrtc/voiceengine/WebRtcAudioManager");
const WEBRTC_AUDIO_UTILS_CLASS: &JNIStr = jni_str!("org/webrtc/voiceengine/WebRtcAudioUtils");
const PROCESS_CLASS: &JNIStr = jni_str!("android/os/Process");
const RECORD_AUDIO: &str = "android.permission.RECORD_AUDIO";
const MODE_NORMAL: jint = 0;
const CACHE_AUDIO_PARAMETERS_NAME: &JNIStr = jni_str!("nativeCacheAudioParameters");
const CACHE_AUDIO_PARAMETERS_SIG: &JNIStr = jni_str!("(IIIZZZZZZZIIJ)V");
const NATIVE_AUDIO_MANAGER: jlong = 0x5eed_a0d1;

static CACHED_AUDIO_PARAMETERS: Mutex<Option<CachedAudioParameters>> = Mutex::new(None);

struct CachedAudioParameters {
    sample_rate: jint,
    output_channels: jint,
    input_channels: jint,
    hardware_aec: bool,
    hardware_agc: bool,
    hardware_ns: bool,
    low_latency_output: bool,
    low_latency_input: bool,
    pro_audio: bool,
    aaudio: bool,
    output_buffer_frames: jint,
    input_buffer_frames: jint,
    native_audio_manager: jlong,
}

impl fmt::Display for CachedAudioParameters {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cached {} Hz, {} output and {} input channel for native manager {:#x}, hardware AEC \
             {}, AGC {}, NS {}, low-latency output {} and input {}, pro audio {}, AAudio {}, {} \
             output and {} input frames per buffer",
            self.sample_rate,
            self.output_channels,
            self.input_channels,
            self.native_audio_manager,
            self.hardware_aec,
            self.hardware_agc,
            self.hardware_ns,
            self.low_latency_output,
            self.low_latency_input,
            self.pro_audio,
            self.aaudio,
            self.output_buffer_frames,
            self.input_buffer_frames
        )
    }
}

struct WebRtcVoiceReadback {
    parameters: Option<CachedAudioParameters>,
    initialized: bool,
    open_sl_es_blacklisted: bool,
    communication_mode_in_voice_chat: bool,
    communication_mode_after_voice_chat: bool,
    has_microphone: bool,
    record_audio_permission: jint,
}

impl fmt::Display for WebRtcVoiceReadback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WebRtcAudioManager ")?;
        match &self.parameters {
            Some(parameters) => write!(f, "{parameters}")?,
            None => f.write_str("never cached its audio parameters")?,
        }
        writeln!(
            f,
            "; init returned {}, OpenSL ES blacklisted {}",
            self.initialized, self.open_sl_es_blacklisted
        )?;
        writeln!(
            f,
            "WebRtcAudioManager communication mode enabled {} after the client set \
             MODE_IN_COMMUNICATION and {} after another AudioManager set MODE_NORMAL",
            self.communication_mode_in_voice_chat, self.communication_mode_after_voice_chat
        )?;
        write!(
            f,
            "WebRtcAudioUtils.hasMicrophone() returned {}; Context.checkPermission({RECORD_AUDIO}) \
             returned {}",
            self.has_microphone, self.record_audio_permission
        )
    }
}

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
enum DexBuffer {
    Heap,
    Direct,
}

enum DexLoadOutcome {
    DefinedByLoader,
    DefinedByAnotherLoader,
    Threw(String),
}

struct InMemoryDexLoad {
    buffer: DexBuffer,
    outcome: DexLoadOutcome,
}

impl fmt::Display for InMemoryDexLoad {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let buffer = match self.buffer {
            DexBuffer::Heap => "heap",
            DexBuffer::Direct => "direct",
        };
        write!(f, "InMemoryDexClassLoader over a {buffer} buffer ")?;
        match &self.outcome {
            DexLoadOutcome::DefinedByLoader => write!(f, "defined {PROBE_DEX_CLASS}"),
            DexLoadOutcome::DefinedByAnotherLoader => {
                write!(f, "loaded {PROBE_DEX_CLASS} from another class loader")
            }
            DexLoadOutcome::Threw(exception) => write!(f, "threw {exception}"),
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
    appsflyer_statics: AppsFlyerStatics,
    in_memory_dex: [InMemoryDexLoad; 2],
    corrupt_dex: NativeOutcome,
    notification_builder: i64,
    opened_link: NativeOutcome,
    refused_link: NativeOutcome,
    shared_text: NativeOutcome,
    host_clipboard: Option<String>,
    second_finger: TouchEventReadback,
    point_move: PointEventReadback,
    webrtc_voice: WebRtcVoiceReadback,
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
        writeln!(f, "{}", self.appsflyer_statics)?;
        for load in &self.in_memory_dex {
            writeln!(f, "{load}")?;
        }
        writeln!(
            f,
            "new DexFile(ByteBuffer) over a dex with a bad checksum {}",
            self.corrupt_dex
        )?;
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
        writeln!(
            f,
            "MotionEvent.obtain for a second finger returned {}",
            self.second_finger
        )?;
        writeln!(
            f,
            "MotionEvent.obtain for a pointer move returned {}",
            self.point_move
        )?;
        writeln!(f, "{}", self.webrtc_voice)?;
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
    let appsflyer_statics = appsflyer_statics::probe(env)?;
    let in_memory_dex = in_memory_dex_loads(env)?;
    let corrupt_dex = corrupt_dex_file(env)?;
    let notification_builder = client_notifications(env)?;
    let opened_link = open_uri(env, OPENED_LINK)?;
    let refused_link = open_uri(env, REFUSED_LINK)?;
    let shared_text = share_text(env, SHARED_TEXT)?;
    let second_finger = probe_second_finger(env)?;
    let point_move = probe_point_move(env)?;
    let webrtc_voice = webrtc_voice(env)?;
    Ok(PlatformProbeReport {
        pictures,
        movies,
        temporary,
        keep_screen_on,
        appsflyer_statics,
        in_memory_dex,
        corrupt_dex,
        notification_builder,
        opened_link,
        refused_link,
        shared_text,
        host_clipboard: take_pending_host_clipboard_text(),
        second_finger,
        point_move,
        webrtc_voice,
    })
}

fn webrtc_voice(env: &mut Env) -> Result<WebRtcVoiceReadback, FrameworkError> {
    let context = checked(env, "ATLLoadedApp primary Context", |env| {
        let loaded_app = env
            .call_static_method(
                ATL_LOADED_APP_CLASS,
                jni_str!("getPrimaryApplication"),
                jni_sig!("()Landroid/atl/ATLLoadedApp;"),
                &[],
            )?
            .l()?;
        env.call_method(
            &loaded_app,
            jni_str!("createContext"),
            jni_sig!(
                "(Landroid/util/DisplayMetrics;Landroid/content/res/Configuration;I)Landroid/app/ContextImpl;"
            ),
            &[
                JValue::Object(&JObject::null()),
                JValue::Object(&JObject::null()),
                JValue::Int(0),
            ],
        )?
        .l()
    })?;
    checked(env, "ContextUtils.initialize", |env| {
        env.call_static_method(
            CONTEXT_UTILS_CLASS,
            jni_str!("initialize"),
            jni_sig!("(Landroid/content/Context;)V"),
            &[JValue::Object(&context)],
        )?
        .v()
    })?;
    let has_microphone = checked(env, "WebRtcAudioUtils.hasMicrophone", |env| {
        env.call_static_method(
            WEBRTC_AUDIO_UTILS_CLASS,
            jni_str!("hasMicrophone"),
            jni_sig!("()Z"),
            &[],
        )?
        .z()
    })?;
    let record_audio_permission = record_audio_permission(env, &context)?;
    checked(
        env,
        "RegisterNatives WebRtcAudioManager.nativeCacheAudioParameters",
        |env| {
            let class = env.find_class(WEBRTC_AUDIO_MANAGER_CLASS)?;
            let methods = [unsafe {
                NativeMethod::from_raw_parts(
                    CACHE_AUDIO_PARAMETERS_NAME,
                    CACHE_AUDIO_PARAMETERS_SIG,
                    native_cache_audio_parameters as *mut c_void,
                )
            }];
            unsafe { env.register_native_methods(&class, &methods) }
        },
    )?;
    let manager = checked(env, "NewObject WebRtcAudioManager(J)V", |env| {
        env.new_object(
            WEBRTC_AUDIO_MANAGER_CLASS,
            jni_sig!("(J)V"),
            &[JValue::Long(NATIVE_AUDIO_MANAGER)],
        )
    })?;
    let parameters = CACHED_AUDIO_PARAMETERS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    let mut boolean = |name: &'static JNIStr| {
        checked(env, "WebRtcAudioManager boolean method", |env| {
            env.call_method(&manager, name, jni_sig!("()Z"), &[])?.z()
        })
    };
    let initialized = boolean(jni_str!("init"))?;
    let communication_mode_in_voice_chat = boolean(jni_str!("isCommunicationModeEnabled"))?;
    let open_sl_es_blacklisted = boolean(jni_str!("isDeviceBlacklistedForOpenSLESUsage"))?;
    for mute in [true, false] {
        checked(env, "WebRtcAudioManager.setMicrophoneMute", |env| {
            env.call_method(
                &manager,
                jni_str!("setMicrophoneMute"),
                jni_sig!("(Z)V"),
                &[JValue::Bool(mute)],
            )?
            .v()
        })?;
    }
    checked(env, "WebRtcAudioManager.dispose", |env| {
        env.call_method(&manager, jni_str!("dispose"), jni_sig!("()V"), &[])?
            .v()
    })?;
    restore_normal_audio_mode(env, &context)?;
    let communication_mode_after_voice_chat = checked(
        env,
        "WebRtcAudioManager.isCommunicationModeEnabled after MODE_NORMAL",
        |env| {
            env.call_method(
                &manager,
                jni_str!("isCommunicationModeEnabled"),
                jni_sig!("()Z"),
                &[],
            )?
            .z()
        },
    )?;
    Ok(WebRtcVoiceReadback {
        parameters,
        initialized,
        open_sl_es_blacklisted,
        communication_mode_in_voice_chat,
        communication_mode_after_voice_chat,
        has_microphone,
        record_audio_permission,
    })
}

fn restore_normal_audio_mode(env: &mut Env, context: &JObject) -> Result<(), FrameworkError> {
    let service = env.new_string("audio")?;
    checked(env, "AudioManager.setMode(MODE_NORMAL)", |env| {
        let audio_manager = env
            .call_method(
                context,
                jni_str!("getSystemService"),
                jni_sig!("(Ljava/lang/String;)Ljava/lang/Object;"),
                &[JValue::Object(&service)],
            )?
            .l()?;
        env.call_method(
            &audio_manager,
            jni_str!("setMode"),
            jni_sig!("(I)V"),
            &[JValue::Int(MODE_NORMAL)],
        )?
        .v()
    })
}

fn record_audio_permission(env: &mut Env, context: &JObject) -> Result<jint, FrameworkError> {
    let permission = env.new_string(RECORD_AUDIO)?;
    checked(env, "Context.checkPermission(RECORD_AUDIO)", |env| {
        let pid = env
            .call_static_method(PROCESS_CLASS, jni_str!("myPid"), jni_sig!("()I"), &[])?
            .i()?;
        let uid = env
            .call_static_method(PROCESS_CLASS, jni_str!("myUid"), jni_sig!("()I"), &[])?
            .i()?;
        env.call_method(
            context,
            jni_str!("checkPermission"),
            jni_sig!("(Ljava/lang/String;II)I"),
            &[
                JValue::Object(&permission),
                JValue::Int(pid),
                JValue::Int(uid),
            ],
        )?
        .i()
    })
}

extern "system" fn native_cache_audio_parameters<'local>(
    mut env: EnvUnowned<'local>,
    _manager: JObject<'local>,
    sample_rate: jint,
    output_channels: jint,
    input_channels: jint,
    hardware_aec: jboolean,
    hardware_agc: jboolean,
    hardware_ns: jboolean,
    low_latency_output: jboolean,
    low_latency_input: jboolean,
    pro_audio: jboolean,
    aaudio: jboolean,
    output_buffer_frames: jint,
    input_buffer_frames: jint,
    native_audio_manager: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        *CACHED_AUDIO_PARAMETERS
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(CachedAudioParameters {
            sample_rate,
            output_channels,
            input_channels,
            hardware_aec,
            hardware_agc,
            hardware_ns,
            low_latency_output,
            low_latency_input,
            pro_audio,
            aaudio,
            output_buffer_frames,
            input_buffer_frames,
            native_audio_manager,
        });
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
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
    match called {
        Ok(_) => Ok(NativeOutcome::Returned),
        Err(error) => caught_exception(env, error).map(NativeOutcome::Threw),
    }
}

fn caught_exception(env: &mut Env, error: jni::errors::Error) -> Result<String, FrameworkError> {
    match env.exception_catch() {
        Err(jni::errors::Error::CaughtJavaException { name, .. }) => Ok(name),
        Ok(()) | Err(_) => Err(FrameworkError::Jni(error)),
    }
}

fn in_memory_dex_loads(env: &mut Env) -> Result<[InMemoryDexLoad; 2], FrameworkError> {
    let dex = probe_dex();
    let padding = [0; PROBE_DEX_PADDING];
    let padded = [&padding[..], &dex, &padding].concat();
    let start = PROBE_DEX_PADDING as jint;
    let end = start + dex.len() as jint;
    let bytes = env.byte_array_from_slice(&padded)?;
    let heap = checked(env, "ByteBuffer.wrap(byte[], int, int)", |env| {
        env.call_static_method(
            BYTE_BUFFER_CLASS,
            jni_str!("wrap"),
            jni_sig!("([BII)Ljava/nio/ByteBuffer;"),
            &[
                JValue::Object(&bytes),
                JValue::Int(start),
                JValue::Int(end - start),
            ],
        )?
        .l()
    })?;
    let direct = checked(env, "ByteBuffer.allocateDirect", |env| {
        let buffer = env
            .call_static_method(
                BYTE_BUFFER_CLASS,
                jni_str!("allocateDirect"),
                jni_sig!("(I)Ljava/nio/ByteBuffer;"),
                &[JValue::Int(padded.len() as jint)],
            )?
            .l()?;
        env.call_method(
            &buffer,
            jni_str!("put"),
            jni_sig!("([B)Ljava/nio/ByteBuffer;"),
            &[JValue::Object(&bytes)],
        )?;
        env.call_method(
            &buffer,
            jni_str!("limit"),
            jni_sig!("(I)Ljava/nio/Buffer;"),
            &[JValue::Int(end)],
        )?;
        env.call_method(
            &buffer,
            jni_str!("position"),
            jni_sig!("(I)Ljava/nio/Buffer;"),
            &[JValue::Int(start)],
        )?;
        Ok(buffer)
    })?;
    Ok([
        load_probe_dex(env, DexBuffer::Heap, &heap)?,
        load_probe_dex(env, DexBuffer::Direct, &direct)?,
    ])
}

fn load_probe_dex(
    env: &mut Env,
    buffer: DexBuffer,
    dex: &JObject,
) -> Result<InMemoryDexLoad, FrameworkError> {
    let parent = checked(env, "ClassLoader.getSystemClassLoader", |env| {
        env.call_static_method(
            CLASS_LOADER_CLASS,
            jni_str!("getSystemClassLoader"),
            jni_sig!("()Ljava/lang/ClassLoader;"),
            &[],
        )?
        .l()
    })?;
    let name = env.new_string(PROBE_DEX_CLASS)?;
    let outcome = match defined_by_new_loader(env, dex, &parent, &name) {
        Ok(true) => DexLoadOutcome::DefinedByLoader,
        Ok(false) => DexLoadOutcome::DefinedByAnotherLoader,
        Err(error) => DexLoadOutcome::Threw(caught_exception(env, error)?),
    };
    Ok(InMemoryDexLoad { buffer, outcome })
}

fn defined_by_new_loader(
    env: &mut Env,
    dex: &JObject,
    parent: &JObject,
    name: &JObject,
) -> Result<bool, jni::errors::Error> {
    let loader = env.new_object(
        IN_MEMORY_DEX_CLASS_LOADER_CLASS,
        jni_sig!("(Ljava/nio/ByteBuffer;Ljava/lang/ClassLoader;)V"),
        &[JValue::Object(dex), JValue::Object(parent)],
    )?;
    let class = env
        .call_method(
            &loader,
            jni_str!("loadClass"),
            jni_sig!("(Ljava/lang/String;)Ljava/lang/Class;"),
            &[JValue::Object(name)],
        )?
        .l()?;
    let defining_loader = env
        .call_method(
            &class,
            jni_str!("getClassLoader"),
            jni_sig!("()Ljava/lang/ClassLoader;"),
            &[],
        )?
        .l()?;
    env.is_same_object(&defining_loader, &loader)
}

fn corrupt_dex_file(env: &mut Env) -> Result<NativeOutcome, FrameworkError> {
    let mut dex = probe_dex();
    dex[DEX_CHECKSUM_OFFSET] ^= 0xff;
    let bytes = env.byte_array_from_slice(&dex)?;
    let buffer = checked(env, "ByteBuffer.wrap(byte[])", |env| {
        env.call_static_method(
            BYTE_BUFFER_CLASS,
            jni_str!("wrap"),
            jni_sig!("([B)Ljava/nio/ByteBuffer;"),
            &[JValue::Object(&bytes)],
        )?
        .l()
    })?;
    let called = env.new_object(
        DEX_FILE_CLASS,
        jni_sig!("(Ljava/nio/ByteBuffer;)V"),
        &[JValue::Object(&buffer)],
    );
    outcome(env, called)
}

fn probe_dex() -> Vec<u8> {
    let probe_descriptor = format!("L{};", PROBE_DEX_CLASS.replace('.', "/"));
    let descriptors = [probe_descriptor.as_str(), OBJECT_DESCRIPTOR];
    let (probe_type, object_type) = (0, 1);
    let id_count = descriptors.len() as u32;
    let string_ids_off = DEX_HEADER_SIZE;
    let type_ids_off = string_ids_off + 4 * id_count;
    let class_defs_off = type_ids_off + 4 * id_count;
    let data_off = class_defs_off + 4 * DEX_CLASS_DEF_WORDS;

    let mut dex = vec![0; data_off as usize];
    let mut string_data_offs = Vec::with_capacity(descriptors.len());
    for descriptor in descriptors {
        string_data_offs.push(dex.len() as u32);
        dex.push(descriptor.len() as u8);
        dex.extend_from_slice(descriptor.as_bytes());
        dex.push(0);
    }
    dex.resize(dex.len().next_multiple_of(4), 0);
    let map_off = dex.len() as u32;
    let map = [
        (DEX_MAP_HEADER_ITEM, 1, 0),
        (DEX_MAP_STRING_ID_ITEM, id_count, string_ids_off),
        (DEX_MAP_TYPE_ID_ITEM, id_count, type_ids_off),
        (DEX_MAP_CLASS_DEF_ITEM, 1, class_defs_off),
        (DEX_MAP_STRING_DATA_ITEM, id_count, data_off),
        (DEX_MAP_MAP_LIST, 1, map_off),
    ];
    dex.extend_from_slice(&(map.len() as u32).to_le_bytes());
    for (kind, count, offset) in map {
        for word in [kind, count, offset] {
            dex.extend_from_slice(&word.to_le_bytes());
        }
    }
    let file_size = dex.len() as u32;

    let header = [
        file_size,
        DEX_HEADER_SIZE,
        DEX_ENDIAN_CONSTANT,
        0,
        0,
        map_off,
        id_count,
        string_ids_off,
        id_count,
        type_ids_off,
        0,
        0,
        0,
        0,
        0,
        0,
        1,
        class_defs_off,
        file_size - data_off,
        data_off,
    ];
    let type_descriptor_strings = [probe_type, object_type];
    let class_def = [
        probe_type,
        DEX_ACC_PUBLIC,
        object_type,
        0,
        DEX_NO_INDEX,
        0,
        0,
        0,
    ];
    let words = header
        .iter()
        .chain(&string_data_offs)
        .chain(&type_descriptor_strings)
        .chain(&class_def);
    let (slots, _) = dex[DEX_FILE_SIZE_OFFSET..data_off as usize].as_chunks_mut::<4>();
    for (slot, word) in slots.iter_mut().zip(words) {
        *slot = word.to_le_bytes();
    }
    dex[..DEX_MAGIC.len()].copy_from_slice(DEX_MAGIC);
    let signature = digest(&SHA1_FOR_LEGACY_USE_ONLY, &dex[DEX_FILE_SIZE_OFFSET..]);
    dex[DEX_SIGNATURE_OFFSET..DEX_FILE_SIZE_OFFSET].copy_from_slice(signature.as_ref());
    let checksum = adler32(&dex[DEX_SIGNATURE_OFFSET..]);
    dex[DEX_CHECKSUM_OFFSET..DEX_SIGNATURE_OFFSET].copy_from_slice(&checksum.to_le_bytes());
    dex
}

fn adler32(bytes: &[u8]) -> u32 {
    let (low, high) = bytes.iter().fold((1, 0), |(low, high), &byte| {
        let low = (low + u32::from(byte)) % ADLER_MODULUS;
        (low, (high + low) % ADLER_MODULUS)
    });
    (high << 16) | low
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

#[cfg(test)]
mod tests {
    use super::adler32;

    #[test]
    fn adler32_matches_the_reference_checksum() {
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }
}
