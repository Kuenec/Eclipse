use std::fmt;

use jni::signature::MethodSignature;
use jni::strings::JNIStr;
use jni::{jni_sig, jni_str, Env, JValue, JValueOwned};

use super::{checked, FrameworkError, PROCESS_CLASS};

const VIEW_CONFIGURATION_CLASS: &JNIStr = jni_str!("android/view/ViewConfiguration");
const AUDIO_TRACK_CLASS: &JNIStr = jni_str!("android/media/AudioTrack");
const DRAWABLE_CLASS: &JNIStr = jni_str!("android/graphics/drawable/Drawable");
const TEXT_UTILS_CLASS: &JNIStr = jni_str!("android/text/TextUtils");
const ANDROID_CHARACTER_CLASS: &JNIStr = jni_str!("android/text/AndroidCharacter");
const IMAGE_FORMAT_CLASS: &JNIStr = jni_str!("android/graphics/ImageFormat");
const CDMA_CELL_LOCATION_CLASS: &JNIStr = jni_str!("android/telephony/cdma/CdmaCellLocation");

const VIEW_CONFIGURATION_INTS: [&JNIStr; 9] = [
    jni_str!("getEdgeSlop"),
    jni_str!("getFadingEdgeLength"),
    jni_str!("getJumpTapTimeout"),
    jni_str!("getMaximumDrawingCacheSize"),
    jni_str!("getMaximumFlingVelocity"),
    jni_str!("getPressedStateDuration"),
    jni_str!("getScrollBarSize"),
    jni_str!("getTouchSlop"),
    jni_str!("getWindowTouchSlop"),
];
const VIEW_CONFIGURATION_LONGS: [&JNIStr; 2] = [
    jni_str!("getGlobalActionKeyTimeout"),
    jni_str!("getZoomControlsTimeout"),
];
const AUDIO_TRACK_FLOATS: [&JNIStr; 2] = [jni_str!("getMinVolume"), jni_str!("getMaxVolume")];
const TEXT_UTILS_OFFSETS: [(&JNIStr, i32); 2] = [
    (jni_str!("getOffsetAfter"), 1),
    (jni_str!("getOffsetBefore"), 3),
];

const OPAQUE: i32 = -1;
const TRANSLUCENT: i32 = -3;
const SURROGATE_PAIR_TEXT: &str = "a\u{1F600}b";
const LEFT_PARENTHESIS: u16 = 0x28;
const NV21: i32 = 0x11;
const QUARTER_SECONDS_PER_DEGREE: i32 = 14400;
const EMPTY_GROUP_NAME: &str = "";

enum Returned {
    Int(i32),
    Long(i64),
    Float(f32),
    Char(u16),
    Double(f64),
}

impl fmt::Display for Returned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Int(value) => write!(f, "{value}"),
            Self::Long(value) => write!(f, "{value}"),
            Self::Float(value) => write!(f, "{value:?}"),
            Self::Char(value) => write!(f, "U+{value:04X}"),
            Self::Double(value) => write!(f, "{value:?}"),
        }
    }
}

pub(crate) struct AppsFlyerStatics(Vec<(String, Returned)>);

impl fmt::Display for AppsFlyerStatics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, (call, returned)) in self.0.iter().enumerate() {
            if index > 0 {
                writeln!(f)?;
            }
            write!(f, "{call} returned {returned}")?;
        }
        Ok(())
    }
}

fn call_static<'local>(
    env: &mut Env<'local>,
    class: &JNIStr,
    name: &JNIStr,
    signature: MethodSignature<'static, 'static>,
    args: &[JValue],
) -> Result<JValueOwned<'local>, FrameworkError> {
    let what = format!("{class}.{name}{}", signature.sig());
    checked(env, &what, |env| {
        env.call_static_method(class, name, signature, args)
    })
}

pub(crate) fn probe(env: &mut Env) -> Result<AppsFlyerStatics, FrameworkError> {
    let mut returned = Vec::new();
    for name in VIEW_CONFIGURATION_INTS {
        let value = call_static(env, VIEW_CONFIGURATION_CLASS, name, jni_sig!("()I"), &[])?.i()?;
        returned.push((format!("ViewConfiguration.{name}()"), Returned::Int(value)));
    }
    for name in VIEW_CONFIGURATION_LONGS {
        let value = call_static(env, VIEW_CONFIGURATION_CLASS, name, jni_sig!("()J"), &[])?.j()?;
        returned.push((format!("ViewConfiguration.{name}()"), Returned::Long(value)));
    }
    for name in AUDIO_TRACK_FLOATS {
        let value = call_static(env, AUDIO_TRACK_CLASS, name, jni_sig!("()F"), &[])?.f()?;
        returned.push((format!("AudioTrack.{name}()"), Returned::Float(value)));
    }
    let opacity = call_static(
        env,
        DRAWABLE_CLASS,
        jni_str!("resolveOpacity"),
        jni_sig!("(II)I"),
        &[JValue::Int(OPAQUE), JValue::Int(TRANSLUCENT)],
    )?
    .i()?;
    returned.push((
        format!("Drawable.resolveOpacity({OPAQUE}, {TRANSLUCENT})"),
        Returned::Int(opacity),
    ));
    let text = env.new_string(SURROGATE_PAIR_TEXT)?;
    for (name, offset) in TEXT_UTILS_OFFSETS {
        let value = call_static(
            env,
            TEXT_UTILS_CLASS,
            name,
            jni_sig!("(Ljava/lang/CharSequence;I)I"),
            &[JValue::Object(&text), JValue::Int(offset)],
        )?
        .i()?;
        returned.push((
            format!("TextUtils.{name}({SURROGATE_PAIR_TEXT:?}, {offset})"),
            Returned::Int(value),
        ));
    }
    let mirror = call_static(
        env,
        ANDROID_CHARACTER_CLASS,
        jni_str!("getMirror"),
        jni_sig!("(C)C"),
        &[JValue::Char(LEFT_PARENTHESIS)],
    )?
    .c()?;
    returned.push((
        format!("AndroidCharacter.getMirror(U+{LEFT_PARENTHESIS:04X})"),
        Returned::Char(mirror),
    ));
    let bits = call_static(
        env,
        IMAGE_FORMAT_CLASS,
        jni_str!("getBitsPerPixel"),
        jni_sig!("(I)I"),
        &[JValue::Int(NV21)],
    )?
    .i()?;
    returned.push((
        format!("ImageFormat.getBitsPerPixel({NV21:#x})"),
        Returned::Int(bits),
    ));
    let degrees = call_static(
        env,
        CDMA_CELL_LOCATION_CLASS,
        jni_str!("convertQuartSecToDecDegrees"),
        jni_sig!("(I)D"),
        &[JValue::Int(QUARTER_SECONDS_PER_DEGREE)],
    )?
    .d()?;
    returned.push((
        format!("CdmaCellLocation.convertQuartSecToDecDegrees({QUARTER_SECONDS_PER_DEGREE})"),
        Returned::Double(degrees),
    ));
    let group_name = env.new_string(EMPTY_GROUP_NAME)?;
    let gid = call_static(
        env,
        PROCESS_CLASS,
        jni_str!("getGidForName"),
        jni_sig!("(Ljava/lang/String;)I"),
        &[JValue::Object(&group_name)],
    )?
    .i()?;
    returned.push((
        format!("Process.getGidForName({EMPTY_GROUP_NAME:?})"),
        Returned::Int(gid),
    ));
    Ok(AppsFlyerStatics(returned))
}
