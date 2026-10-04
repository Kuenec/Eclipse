use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use jni::ids::{JFieldID, JMethodID, JStaticMethodID};
use jni::objects::{JClass, JObject, JObjectArray};
use jni::refs::Global;
use jni::signature::{JavaType, MethodSignature, Primitive};
use jni::strings::JNIStr;
use jni::sys::jvalue;
use jni::vm::JavaVM;
use jni::{jni_sig, jni_str, Env, JValue};
use winit::event::TouchPhase;

use super::{
    checked, EngineTouchOutcome, FrameworkError, MOTION_EVENT_CLASS, NATIVE_INPUT_INTERFACE_CLASS,
};
use crate::gamepad::GamepadCall;
use crate::input::{TouchMotion, TouchPointer, TouchTracker, MAX_TOUCH_POINTERS};
use crate::runtime::Vm;

struct GamepadNatives {
    class: Global<JClass<'static>>,
    supported_key: JStaticMethodID,
    supported_motion: JStaticMethodID,
    connect: JStaticMethodID,
    disconnect: JStaticMethodID,
    button: JStaticMethodID,
    axis: JStaticMethodID,
}

static GAMEPAD_NATIVES: OnceLock<GamepadNatives> = OnceLock::new();

#[derive(Debug)]
pub(crate) enum GamepadPathError {
    Framework(FrameworkError),
    MissingNative {
        name: &'static JNIStr,
        error: jni::errors::Error,
    },
}

impl fmt::Display for GamepadPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Framework(error) => error.fmt(f),
            Self::MissingNative { name, error } => write!(
                f,
                "this Roblox client has no NativeInputInterface.{name}: {error}"
            ),
        }
    }
}

impl From<jni::errors::Error> for GamepadPathError {
    fn from(error: jni::errors::Error) -> Self {
        Self::Framework(FrameworkError::Jni(error))
    }
}

pub(crate) fn pass_gamepad_calls(vm: &Vm, calls: &[GamepadCall]) -> Result<(), GamepadPathError> {
    if calls.is_empty() {
        return Ok(());
    }
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(GamepadPathError::Framework(FrameworkError::NullVm));
    }
    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| pass_calls(env, calls))) {
            Ok(result) => result,
            Err(_) => Err(GamepadPathError::Framework(FrameworkError::Panicked)),
        }
    })
}

fn pass_calls(env: &mut Env, calls: &[GamepadCall]) -> Result<(), GamepadPathError> {
    let natives = gamepad_natives(env)?;
    for call in calls {
        let int = |value: i32| JValue::Int(value).as_jni();
        let boolean = |value: bool| JValue::Bool(value).as_jni();
        let float = |value: f32| JValue::Float(value).as_jni();
        let passed = match *call {
            GamepadCall::SupportedKey {
                device,
                key_code,
                supported,
                kind,
            } => natives.call(
                env,
                natives.supported_key,
                &[
                    int(device.get()),
                    int(key_code),
                    boolean(supported),
                    int(kind.engine_type()),
                ],
            ),
            GamepadCall::SupportedMotion {
                device,
                axis,
                direction,
                supported,
                kind,
            } => natives.call(
                env,
                natives.supported_motion,
                &[
                    int(device.get()),
                    int(axis),
                    int(direction),
                    boolean(supported),
                    int(kind.engine_type()),
                ],
            ),
            GamepadCall::Connect { device, kind } => natives.call(
                env,
                natives.connect,
                &[int(device.get()), int(kind.engine_type())],
            ),
            GamepadCall::Disconnect { device } => {
                natives.call(env, natives.disconnect, &[int(device.get())])
            }
            GamepadCall::Button {
                device,
                key_code,
                down,
            } => natives.call(
                env,
                natives.button,
                &[int(device.get()), int(key_code), int(i32::from(down))],
            ),
            GamepadCall::Axis {
                device,
                axis,
                x,
                y,
                value,
            } => natives.call(
                env,
                natives.axis,
                &[
                    int(device.get()),
                    int(axis),
                    float(x),
                    float(y),
                    float(value),
                ],
            ),
        };
        match passed {
            Ok(()) => {
                static PATH_LOGGED: AtomicBool = AtomicBool::new(false);
                if !PATH_LOGGED.swap(true, Ordering::Relaxed) {
                    tracing::info!("gamepad input path active");
                }
                tracing::trace!(?call, "gamepad call passed to the engine");
            }
            Err(error) => tracing::warn!(%error, ?call, "gamepad call threw (ignored)"),
        }
    }
    Ok(())
}

impl GamepadNatives {
    fn call(
        &self,
        env: &mut Env,
        method: JStaticMethodID,
        args: &[jvalue],
    ) -> Result<(), FrameworkError> {
        checked(env, "NativeInputInterface gamepad native", |env| {
            unsafe {
                env.call_static_method_unchecked(
                    &self.class,
                    method,
                    JavaType::Primitive(Primitive::Void),
                    args,
                )
            }?
            .v()
        })
    }
}

fn gamepad_natives(env: &mut Env) -> Result<&'static GamepadNatives, GamepadPathError> {
    if let Some(natives) = GAMEPAD_NATIVES.get() {
        return Ok(natives);
    }
    let class = env
        .find_class(NATIVE_INPUT_INTERFACE_CLASS)
        .inspect_err(|_| {
            env.exception_clear();
        })?;
    let natives = GamepadNatives {
        supported_key: static_method(
            env,
            &class,
            jni_str!("nativeSetGamepadSupportedKeyWithGamepadType"),
            jni_sig!("(IIZI)V"),
        )?,
        supported_motion: static_method(
            env,
            &class,
            jni_str!("nativeSetGamepadSupportedMotionWithGamepadType"),
            jni_sig!("(IIIZI)V"),
        )?,
        connect: static_method(
            env,
            &class,
            jni_str!("nativeGamepadConnectEventWithGamepadType"),
            jni_sig!("(II)V"),
        )?,
        disconnect: static_method(
            env,
            &class,
            jni_str!("nativeGamepadDisconnectEvent"),
            jni_sig!("(I)V"),
        )?,
        button: static_method(
            env,
            &class,
            jni_str!("nativeGamepadButtonEvent"),
            jni_sig!("(III)V"),
        )?,
        axis: static_method(
            env,
            &class,
            jni_str!("nativeGamepadAxisEvent"),
            jni_sig!("(IIFFF)V"),
        )?,
        class: env.new_global_ref(&class)?,
    };
    Ok(GAMEPAD_NATIVES.get_or_init(|| natives))
}

fn static_method(
    env: &mut Env,
    class: &JClass,
    name: &'static JNIStr,
    signature: MethodSignature<'static, 'static>,
) -> Result<JStaticMethodID, GamepadPathError> {
    env.get_static_method_id(class, name, &signature)
        .map_err(|error| {
            env.exception_clear();
            GamepadPathError::MissingNative { name, error }
        })
}

const SOURCE_TOUCHSCREEN: i32 = 0x1002;

const TOUCHSCREEN_DEVICE_ID: i32 = 1;

const NO_FLAGS: i32 = 0;

const PIXEL_PRECISION: f32 = 1.0;

const MOTION_EVENT_OBTAIN_POINTERS_SIG: MethodSignature<'static, 'static> = jni_sig!(
    "(JJII[Landroid/view/MotionEvent$PointerProperties;[Landroid/view/MotionEvent$PointerCoords;IIFFIIII)Landroid/view/MotionEvent;"
);

struct TouchMotionJni {
    motion_event: Global<JClass<'static>>,
    obtain: JStaticMethodID,
    recycle: JMethodID,
    on_touch_event_internal: JMethodID,
    pointer_id: JFieldID,
    coord_x: JFieldID,
    coord_y: JFieldID,
    property_array: Global<JObjectArray<'static>>,
    coord_array: Global<JObjectArray<'static>>,
    properties: Vec<Global<JObject<'static>>>,
    coords: Vec<Global<JObject<'static>>>,
}

static TOUCH_MOTION_JNI: OnceLock<TouchMotionJni> = OnceLock::new();

pub(crate) fn dispatch_touch_motion(
    vm: &Vm,
    motion: &TouchMotion,
    down_time_ms: Option<i64>,
) -> Result<Option<EngineTouchOutcome>, FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }
    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| {
            touch_engine_surface(env, motion, down_time_ms)
        })) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

fn touch_engine_surface(
    env: &mut Env,
    motion: &TouchMotion,
    down_time_ms: Option<i64>,
) -> Result<Option<EngineTouchOutcome>, FrameworkError> {
    let Some(handle) = super::engine_surface_view_handle() else {
        tracing::debug!(
            action = ?motion.action,
            "engine touch: no RBXSurfaceView holds the engine surface yet (no-op)"
        );
        return Ok(None);
    };
    let Some(surface) = super::engine_surface_view(env, handle)? else {
        return Ok(None);
    };
    let jni = touch_motion_jni(env)?;
    let event_time = super::monotonic_millis();
    let down_time = down_time_ms.unwrap_or(event_time);
    let event = obtain_touch_event(env, jni, motion, down_time, event_time)?;
    let consumed = checked(env, "RBXSurfaceView.onTouchEventInternal", |env| {
        unsafe {
            env.call_method_unchecked(
                &surface,
                jni.on_touch_event_internal,
                JavaType::Primitive(Primitive::Boolean),
                &[
                    JValue::Object(&event).as_jni(),
                    JValue::Bool(false).as_jni(),
                ],
            )
        }?
        .z()
    });
    if let Err(error) = recycle_touch_event(env, jni, &event) {
        tracing::debug!(%error, "MotionEvent.recycle failed (ignored)");
    }
    Ok(Some(EngineTouchOutcome {
        consumed: consumed?,
        down_time_ms: down_time,
    }))
}

fn obtain_touch_event<'local>(
    env: &mut Env<'local>,
    jni: &TouchMotionJni,
    motion: &TouchMotion,
    down_time: i64,
    event_time: i64,
) -> Result<JObject<'local>, FrameworkError> {
    let pointers = motion.pointers();
    for ((pointer, properties), coords) in pointers.iter().zip(&jni.properties).zip(&jni.coords) {
        checked(env, "MotionEvent pointer slots", |env| unsafe {
            env.set_field_unchecked(properties, jni.pointer_id, JValue::Int(pointer.id))?;
            env.set_field_unchecked(coords, jni.coord_x, JValue::Float(pointer.x))?;
            env.set_field_unchecked(coords, jni.coord_y, JValue::Float(pointer.y))
        })?;
    }
    let int = |value: i32| JValue::Int(value).as_jni();
    let precision = JValue::Float(PIXEL_PRECISION).as_jni();
    let args = [
        JValue::Long(down_time).as_jni(),
        JValue::Long(event_time).as_jni(),
        int(motion.action.android_action()),
        int(pointers.len() as i32),
        JValue::Object(&jni.property_array).as_jni(),
        JValue::Object(&jni.coord_array).as_jni(),
        int(NO_FLAGS),
        int(NO_FLAGS),
        precision,
        precision,
        int(TOUCHSCREEN_DEVICE_ID),
        int(NO_FLAGS),
        int(SOURCE_TOUCHSCREEN),
        int(NO_FLAGS),
    ];
    checked(env, "MotionEvent.obtain for touchscreen pointers", |env| {
        unsafe {
            env.call_static_method_unchecked(&jni.motion_event, jni.obtain, JavaType::Object, &args)
        }?
        .l()
    })
}

fn recycle_touch_event(
    env: &mut Env,
    jni: &TouchMotionJni,
    event: &JObject,
) -> Result<(), FrameworkError> {
    checked(env, "MotionEvent.recycle", |env| {
        unsafe {
            env.call_method_unchecked(
                event,
                jni.recycle,
                JavaType::Primitive(Primitive::Void),
                &[],
            )
        }?
        .v()
    })
}

const PROBE_FINGERS: [(u64, f32, f32); 2] = [(11, 10.5, 20.25), (12, 30.75, 40.5)];

const PROBE_DOWN_TIME_MS: i64 = 1000;

const PROBE_EVENT_TIME_MS: i64 = 1016;

pub(crate) struct TouchEventReadback {
    action: i32,
    source: i32,
    down_time: i64,
    event_time: i64,
    pointers: Vec<TouchPointer>,
}

impl fmt::Display for TouchEventReadback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "MotionEvent.obtain for a second finger returned action {:#x}, source {:#x}, down \
             {} ms, event {} ms",
            self.action, self.source, self.down_time, self.event_time
        )?;
        for TouchPointer { id, x, y } in &self.pointers {
            write!(f, ", pointer {id} at ({x}, {y})")?;
        }
        Ok(())
    }
}

pub(crate) fn probe_second_finger(env: &mut Env) -> Result<TouchEventReadback, FrameworkError> {
    let mut tracker = TouchTracker::default();
    let motion = PROBE_FINGERS
        .into_iter()
        .flat_map(|(finger, x, y)| tracker.touch(finger, TouchPhase::Started, x, y))
        .last()
        .expect("a finger press always yields a motion");
    let jni = touch_motion_jni(env)?;
    let event = obtain_touch_event(env, jni, &motion, PROBE_DOWN_TIME_MS, PROBE_EVENT_TIME_MS)?;
    let readback = read_touch_event(env, &event);
    recycle_touch_event(env, jni, &event)?;
    readback
}

fn read_touch_event(env: &mut Env, event: &JObject) -> Result<TouchEventReadback, FrameworkError> {
    let mut int = |name: &'static JNIStr| {
        checked(env, "MotionEvent int getter", |env| {
            env.call_method(event, name, jni_sig!("()I"), &[])?.i()
        })
    };
    let action = int(jni_str!("getAction"))?;
    let source = int(jni_str!("getSource"))?;
    let count = int(jni_str!("getPointerCount"))?;
    let mut long = |name: &'static JNIStr| {
        checked(env, "MotionEvent time getter", |env| {
            env.call_method(event, name, jni_sig!("()J"), &[])?.j()
        })
    };
    let down_time = long(jni_str!("getDownTime"))?;
    let event_time = long(jni_str!("getEventTime"))?;
    let pointers = (0..count)
        .map(|index| {
            checked(env, "MotionEvent pointer getters", |env| {
                let index = [JValue::Int(index)];
                Ok(TouchPointer {
                    id: env
                        .call_method(event, jni_str!("getPointerId"), jni_sig!("(I)I"), &index)?
                        .i()?,
                    x: env
                        .call_method(event, jni_str!("getX"), jni_sig!("(I)F"), &index)?
                        .f()?,
                    y: env
                        .call_method(event, jni_str!("getY"), jni_sig!("(I)F"), &index)?
                        .f()?,
                })
            })
        })
        .collect::<Result<_, _>>()?;
    Ok(TouchEventReadback {
        action,
        source,
        down_time,
        event_time,
        pointers,
    })
}

fn touch_motion_jni(env: &mut Env) -> Result<&'static TouchMotionJni, FrameworkError> {
    if let Some(jni) = TOUCH_MOTION_JNI.get() {
        return Ok(jni);
    }
    let jni = checked(
        env,
        "MotionEvent touchscreen class, method and field IDs",
        |env| {
            let motion_event = env.find_class(MOTION_EVENT_CLASS)?;
            let property_class =
                env.find_class(jni_str!("android/view/MotionEvent$PointerProperties"))?;
            let coord_class = env.find_class(jni_str!("android/view/MotionEvent$PointerCoords"))?;
            let view = env.find_class(jni_str!("android/view/View"))?;
            let property_array =
                env.new_object_array(MAX_TOUCH_POINTERS as i32, &property_class, JObject::null())?;
            let coord_array =
                env.new_object_array(MAX_TOUCH_POINTERS as i32, &coord_class, JObject::null())?;
            let mut properties = Vec::with_capacity(MAX_TOUCH_POINTERS);
            let mut coords = Vec::with_capacity(MAX_TOUCH_POINTERS);
            for index in 0..MAX_TOUCH_POINTERS {
                let property = env.new_object(&property_class, jni_sig!("()V"), &[])?;
                property_array.set_element(env, index, &property)?;
                properties.push(env.new_global_ref(&property)?);
                let coord = env.new_object(&coord_class, jni_sig!("()V"), &[])?;
                coord_array.set_element(env, index, &coord)?;
                coords.push(env.new_global_ref(&coord)?);
            }
            Ok(TouchMotionJni {
                obtain: env.get_static_method_id(
                    &motion_event,
                    jni_str!("obtain"),
                    MOTION_EVENT_OBTAIN_POINTERS_SIG,
                )?,
                recycle: env.get_method_id(&motion_event, jni_str!("recycle"), jni_sig!("()V"))?,
                on_touch_event_internal: env.get_method_id(
                    &view,
                    jni_str!("onTouchEventInternal"),
                    jni_sig!("(Landroid/view/MotionEvent;Z)Z"),
                )?,
                pointer_id: env.get_field_id(&property_class, jni_str!("id"), jni_sig!("I"))?,
                coord_x: env.get_field_id(&coord_class, jni_str!("x"), jni_sig!("F"))?,
                coord_y: env.get_field_id(&coord_class, jni_str!("y"), jni_sig!("F"))?,
                motion_event: env.new_global_ref(&motion_event)?,
                property_array: env.new_global_ref(&property_array)?,
                coord_array: env.new_global_ref(&coord_array)?,
                properties,
                coords,
            })
        },
    )?;
    Ok(TOUCH_MOTION_JNI.get_or_init(|| jni))
}
