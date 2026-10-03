use std::ffi::{c_char, c_int, c_void, CString};
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::OnceLock;
use std::time::Instant;

use jni::errors::LogErrorAndDefault;
use jni::ids::JStaticMethodID;
use jni::objects::{
    JByteArray, JClass, JFloatArray, JIntArray, JLongArray, JMethodID, JObject, JObjectArray,
    JString,
};
use jni::refs::{Global, Reference, Weak};
use jni::signature::{FieldSignature, JavaType, MethodSignature, Primitive};
use jni::strings::{JNIStr, JNIString};
use jni::sys::{jboolean, jfloat, jint, jlong, jshort};
use jni::vm::JavaVM;
use jni::{jni_sig, jni_str, Env, EnvUnowned, JValue, NativeMethod};
use unicode_segmentation::UnicodeSegmentation;

use crate::apk::res_config::ResConfig;
use crate::apk::signature::SigningCertificateHistory;
use crate::runtime::Vm;

mod asset_configuration;
pub mod asset_registry;
pub mod bitmap_registry;
pub mod canvas_registry;
mod captures;
pub mod dialogs;
mod external_intents;
#[cfg(test)]
mod fake_jvm;
pub(crate) mod keep_screen_on;
pub mod lifecycle;
pub mod matrix_registry;
pub(crate) mod memory;
mod message_queue;
pub(crate) mod notifications;
pub mod paint_registry;
pub mod path_registry;
pub mod platform_probe;
mod preloaded_library;
pub(crate) mod roblox_fonts;
mod signing_certificates;
pub mod sqlite;
pub mod theme_registry;
pub mod view_registry;
pub mod window_registry;
pub mod xml_registry;

static CANVAS_DRAW_SUPPORTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

static APK_PATH: OnceLock<String> = OnceLock::new();

const DEFAULT_SCREEN_WIDTH_DP: i32 = 1280;
const DEFAULT_SCREEN_HEIGHT_DP: i32 = 720;

const NATIVE_GET_APK_PATH_NAME: &JNIStr = jni_str!("native_get_apk_path");
const NATIVE_GET_APK_PATH_SIG: &JNIStr = jni_str!("()Ljava/lang/String;");
const NATIVE_UPDATE_CONFIG_NAME: &JNIStr = jni_str!("native_updateConfig");
const NATIVE_UPDATE_CONFIG_SIG: &JNIStr = jni_str!("(Landroid/content/res/Configuration;)V");

extern "system" fn native_get_apk_path<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        let path = APK_PATH
            .get()
            .ok_or(jni::errors::Error::JniCall(jni::errors::JniError::Unknown))?;
        env.new_string(path)
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn native_update_config<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    config: JObject<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let int_sig =
            unsafe { FieldSignature::from_raw_parts(INT_SIG, JavaType::Primitive(Primitive::Int)) };
        env.set_field(
            &config,
            SCREEN_WIDTH_DP_FIELD,
            &int_sig,
            DEFAULT_SCREEN_WIDTH_DP.into(),
        )?;
        env.set_field(
            &config,
            SCREEN_HEIGHT_DP_FIELD,
            &int_sig,
            DEFAULT_SCREEN_HEIGHT_DP.into(),
        )?;
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

const SCREEN_WIDTH_DP_FIELD: &JNIStr = jni_str!("screenWidthDp");
const SCREEN_HEIGHT_DP_FIELD: &JNIStr = jni_str!("screenHeightDp");
const INT_SIG: &JNIStr = jni_str!("I");

const CHAR_SEQUENCE_SIG: &JNIStr = jni_str!("Ljava/lang/CharSequence;");

fn register_context_natives(env: &mut Env, apk_path: &str) -> Result<(), FrameworkError> {
    let _ = APK_PATH.set(apk_path.to_owned());

    let class = env.find_class(CONTEXT_CLASS)?;
    let methods = [
        unsafe {
            NativeMethod::from_raw_parts(
                NATIVE_GET_APK_PATH_NAME,
                NATIVE_GET_APK_PATH_SIG,
                native_get_apk_path as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                NATIVE_UPDATE_CONFIG_NAME,
                NATIVE_UPDATE_CONFIG_SIG,
                native_update_config as *mut std::ffi::c_void,
            )
        },
    ];

    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/content/Context",
        "registered Eclipse's non-GTK backing for native_get_apk_path + native_updateConfig"
    );
    Ok(())
}

pub const LOG_CLASS: &JNIStr = jni_str!("android/util/Log");

const PRINTLN_NATIVE_NAME: &JNIStr = jni_str!("println_native");
const PRINTLN_NATIVE_SIG: &JNIStr = jni_str!("(IILjava/lang/String;Ljava/lang/String;)I");

const LOG_PRIORITY_VERBOSE: jint = 2;
const LOG_PRIORITY_DEBUG: jint = 3;
const LOG_PRIORITY_INFO: jint = 4;
const LOG_PRIORITY_WARN: jint = 5;
const LOG_PRIORITY_ERROR: jint = 6;
const LOG_PRIORITY_ASSERT: jint = 7;

const LOG_ID_MAX: jint = 4;

extern "system" fn println_native<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    buf_id: jint,
    priority: jint,
    tag: JString<'local>,
    msg: JString<'local>,
) -> jint {
    env.with_env(|env| -> jni::errors::Result<jint> {
        if msg.is_null() {
            return Ok(-1);
        }

        if !(0..LOG_ID_MAX).contains(&buf_id) {
            return Ok(-1);
        }

        let tag_str = if tag.is_null() {
            None
        } else {
            Some(tag.try_to_string(env)?)
        };
        let msg_str = msg.try_to_string(env)?;
        let logged = redact_urls_for_log(&msg_str);

        let tag_ref = tag_str.as_deref().unwrap_or("");
        match priority {
            LOG_PRIORITY_VERBOSE => {
                tracing::trace!(target: "android.util.Log", tag = tag_ref, "{logged}")
            }
            LOG_PRIORITY_DEBUG => {
                tracing::debug!(target: "android.util.Log", tag = tag_ref, "{logged}")
            }
            LOG_PRIORITY_INFO => {
                tracing::info!(target: "android.util.Log", tag = tag_ref, "{logged}")
            }
            LOG_PRIORITY_WARN => {
                tracing::warn!(target: "android.util.Log", tag = tag_ref, "{logged}")
            }
            LOG_PRIORITY_ERROR | LOG_PRIORITY_ASSERT => {
                tracing::error!(target: "android.util.Log", tag = tag_ref, "{logged}")
            }

            _ => tracing::info!(target: "android.util.Log", tag = tag_ref, priority, "{logged}"),
        }

        Ok(jint::try_from(msg_str.len()).unwrap_or(jint::MAX))
    })
    .resolve::<LogErrorAndDefault>()
}

fn register_log_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(LOG_CLASS)?;
    let methods = [unsafe {
        NativeMethod::from_raw_parts(
            PRINTLN_NATIVE_NAME,
            PRINTLN_NATIVE_SIG,
            println_native as *mut std::ffi::c_void,
        )
    }];

    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/util/Log",
        "registered Eclipse's non-GTK backing for println_native"
    );
    Ok(())
}

pub const CONNECTIVITY_MANAGER_CLASS: &JNIStr = jni_str!("android/net/ConnectivityManager");

const CM_REGISTER_NETWORK_CALLBACK_NAME: &JNIStr = jni_str!("registerNetworkCallback");
const CM_REGISTER_NETWORK_CALLBACK_SIG: &JNIStr =
    jni_str!("(Landroid/net/NetworkRequest;Landroid/net/ConnectivityManager$NetworkCallback;)V");
const CM_IS_ACTIVE_NETWORK_METERED_NAME: &JNIStr = jni_str!("isActiveNetworkMetered");
const CM_IS_ACTIVE_NETWORK_METERED_SIG: &JNIStr = jni_str!("()Z");
const CM_NATIVE_GET_NETWORK_AVAILABLE_NAME: &JNIStr = jni_str!("nativeGetNetworkAvailable");
const CM_NATIVE_GET_NETWORK_AVAILABLE_SIG: &JNIStr = jni_str!("()Z");

extern "system" fn cm_register_network_callback<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    _request: JObject<'local>,
    _callback: JObject<'local>,
) {
    env.with_env(|_env| -> jni::errors::Result<()> { Ok(()) })
        .resolve::<LogErrorAndDefault>()
}

extern "system" fn cm_is_active_network_metered<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
) -> jboolean {
    env.with_env(|_env| -> jni::errors::Result<jboolean> { Ok(false) })
        .resolve::<LogErrorAndDefault>()
}

extern "system" fn cm_native_get_network_available<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
) -> jboolean {
    env.with_env(|_env| -> jni::errors::Result<jboolean> { Ok(true) })
        .resolve::<LogErrorAndDefault>()
}

fn register_connectivity_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(CONNECTIVITY_MANAGER_CLASS)?;
    let methods = [
        unsafe {
            NativeMethod::from_raw_parts(
                CM_REGISTER_NETWORK_CALLBACK_NAME,
                CM_REGISTER_NETWORK_CALLBACK_SIG,
                cm_register_network_callback as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                CM_IS_ACTIVE_NETWORK_METERED_NAME,
                CM_IS_ACTIVE_NETWORK_METERED_SIG,
                cm_is_active_network_metered as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                CM_NATIVE_GET_NETWORK_AVAILABLE_NAME,
                CM_NATIVE_GET_NETWORK_AVAILABLE_SIG,
                cm_native_get_network_available as *mut std::ffi::c_void,
            )
        },
    ];

    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/net/ConnectivityManager",
        "registered Eclipse's non-GTK backing for registerNetworkCallback (no-op) + isActiveNetworkMetered (false) + nativeGetNetworkAvailable (true)"
    );
    Ok(())
}

pub const ASSET_MANAGER_CLASS: &JNIStr = jni_str!("android/content/res/AssetManager");

const ASSET_MANAGER_INIT_NAME: &JNIStr = jni_str!("init");
const ASSET_MANAGER_INIT_SIG: &JNIStr = jni_str!("(I)V");

const ASSET_MANAGER_SET_APK_ASSETS_NAME: &JNIStr = jni_str!("native_setApkAssets");
const ASSET_MANAGER_SET_APK_ASSETS_SIG: &JNIStr = jni_str!("([Ljava/lang/Object;I)V");

const ASSET_MANAGER_SET_CONFIGURATION_NAME: &JNIStr = jni_str!("setConfiguration");
const ASSET_MANAGER_SET_CONFIGURATION_SIG: &JNIStr =
    jni_str!("(IILjava/lang/String;IIIIIIIIIIIIII)V");

const ASSET_MANAGER_OPEN_XML_ASSET_NAME: &JNIStr = jni_str!("openXmlAssetNative");
const ASSET_MANAGER_OPEN_XML_ASSET_SIG: &JNIStr = jni_str!("(ILjava/lang/String;)J");

const ASSET_MANAGER_RETRIEVE_ATTRIBUTES_NAME: &JNIStr = jni_str!("retrieveAttributes");
const ASSET_MANAGER_RETRIEVE_ATTRIBUTES_SIG: &JNIStr = jni_str!("(J[IIJJ)Z");

const ASSET_MANAGER_NEW_THEME_NAME: &JNIStr = jni_str!("newTheme");
const ASSET_MANAGER_NEW_THEME_SIG: &JNIStr = jni_str!("()J");

const ASSET_MANAGER_DELETE_THEME_NAME: &JNIStr = jni_str!("deleteTheme");
const ASSET_MANAGER_DELETE_THEME_SIG: &JNIStr = jni_str!("(J)V");

const ASSET_MANAGER_APPLY_THEME_STYLE_NAME: &JNIStr = jni_str!("applyThemeStyle");
const ASSET_MANAGER_APPLY_THEME_STYLE_SIG: &JNIStr = jni_str!("(JIZ)V");

const ASSET_MANAGER_COPY_THEME_NAME: &JNIStr = jni_str!("copyTheme");
const ASSET_MANAGER_COPY_THEME_SIG: &JNIStr = jni_str!("(JJ)V");

const ASSET_MANAGER_APPLY_STYLE_NAME: &JNIStr = jni_str!("applyStyle");
const ASSET_MANAGER_APPLY_STYLE_SIG: &JNIStr = jni_str!("(JJII[IIJJ)V");

const ASSET_MANAGER_GET_RESOURCE_NAME_NAME: &JNIStr = jni_str!("getResourceName");
const ASSET_MANAGER_GET_RESOURCE_NAME_SIG: &JNIStr = jni_str!("(I)Ljava/lang/String;");

const ASSET_MANAGER_GET_RESOURCE_PACKAGE_NAME_NAME: &JNIStr = jni_str!("getResourcePackageName");
const ASSET_MANAGER_GET_RESOURCE_PACKAGE_NAME_SIG: &JNIStr = jni_str!("(I)Ljava/lang/String;");

const ASSET_MANAGER_GET_RESOURCE_IDENTIFIER_NAME: &JNIStr = jni_str!("getResourceIdentifier");
const ASSET_MANAGER_GET_RESOURCE_IDENTIFIER_SIG: &JNIStr =
    jni_str!("(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)I");

const ASSET_MANAGER_OPEN_ASSET_NAME: &JNIStr = jni_str!("openAsset");
const ASSET_MANAGER_OPEN_ASSET_SIG: &JNIStr = jni_str!("(Ljava/lang/String;I)J");
const ASSET_MANAGER_READ_ASSET_NAME: &JNIStr = jni_str!("readAsset");

const ASSET_MANAGER_READ_ASSET_SIG: &JNIStr = jni_str!("(J[BJJ)I");

const ASSET_MANAGER_READ_ASSET_CHAR_NAME: &JNIStr = jni_str!("readAssetChar");
const ASSET_MANAGER_READ_ASSET_CHAR_SIG: &JNIStr = jni_str!("(J)I");
const ASSET_MANAGER_SEEK_ASSET_NAME: &JNIStr = jni_str!("seekAsset");
const ASSET_MANAGER_SEEK_ASSET_SIG: &JNIStr = jni_str!("(JJI)J");
const ASSET_MANAGER_GET_ASSET_LENGTH_NAME: &JNIStr = jni_str!("getAssetLength");
const ASSET_MANAGER_GET_ASSET_LENGTH_SIG: &JNIStr = jni_str!("(J)J");
const ASSET_MANAGER_GET_ASSET_REMAINING_LENGTH_NAME: &JNIStr = jni_str!("getAssetRemainingLength");
const ASSET_MANAGER_GET_ASSET_REMAINING_LENGTH_SIG: &JNIStr = jni_str!("(J)J");
const ASSET_MANAGER_DESTROY_ASSET_NAME: &JNIStr = jni_str!("destroyAsset");
const ASSET_MANAGER_DESTROY_ASSET_SIG: &JNIStr = jni_str!("(J)V");

const ASSET_MANAGER_OPEN_ASSET_FD_NAME: &JNIStr = jni_str!("openAssetFd");
const ASSET_MANAGER_OPEN_ASSET_FD_SIG: &JNIStr = jni_str!("(Ljava/lang/String;I[J[J)I");

const ASSET_MANAGER_LOAD_RESOURCE_VALUE_NAME: &JNIStr = jni_str!("loadResourceValue");
const ASSET_MANAGER_LOAD_RESOURCE_VALUE_SIG: &JNIStr = jni_str!("(ISLandroid/util/TypedValue;Z)I");

const ASSET_MANAGER_LOAD_THEME_ATTRIBUTE_VALUE_NAME: &JNIStr = jni_str!("loadThemeAttributeValue");
const ASSET_MANAGER_LOAD_THEME_ATTRIBUTE_VALUE_SIG: &JNIStr =
    jni_str!("(JILandroid/util/TypedValue;Z)I");

const ASSET_MANAGER_GET_POOLED_STRING_NAME: &JNIStr = jni_str!("getPooledString");
const ASSET_MANAGER_GET_POOLED_STRING_SIG: &JNIStr = jni_str!("(II)Ljava/lang/CharSequence;");

const RES_VALUE_TYPE_STRING: u8 = 0x03;

const ECLIPSE_ASSET_COOKIE: jint = 1;

const STYLE_NUM_ENTRIES: usize = 7;

const STYLE_TYPE: usize = 0;

const STYLE_DATA: usize = 1;

const STYLE_ASSET_COOKIE: usize = 2;

const STYLE_RESOURCE_ID: usize = 3;

const XML_BLOCK_COOKIE: i32 = -1;

const NO_ASSET_COOKIE: i32 = -1;

const DATA_NULL_UNDEFINED: i32 = 0;

const TYPE_NULL: i32 = 0;

const TYPE_REFERENCE: u8 = 0x01;

const TYPE_ATTRIBUTE: u8 = 0x02;

const TYPE_STRING: u8 = 0x03;

const ARSC_APP_COOKIE: i32 = 1;

const ARSC_FRAMEWORK_COOKIE: i32 = 2;

fn arsc_cookie_for(resid: u32) -> i32 {
    arsc_cookie_for_package((resid >> 24) as u8)
}

fn arsc_cookie_for_package(package_id: u8) -> i32 {
    if package_id == 0x01 {
        ARSC_FRAMEWORK_COOKIE
    } else {
        ARSC_APP_COOKIE
    }
}

fn arsc_pool_string(cookie: i32, index: u32) -> Option<String> {
    let probe_resid: u32 = match cookie {
        ARSC_FRAMEWORK_COOKIE => 0x0100_0000,
        ARSC_APP_COOKIE => 0x7f00_0000,
        _ => return None,
    };
    let bytes = arsc_bytes_for(probe_resid)?;
    let table = crate::apk::arsc::parse_arsc(bytes).ok()?;
    table.value_string(index).ok().flatten()
}

extern "system" fn asset_manager_init<'local>(
    mut env: EnvUnowned<'local>,
    this: JObject<'local>,
    sdk_version: jint,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        asset_configuration::record(env, &this, asset_configuration::initial(sdk_version))?;
        tracing::debug!(
            target: "android.content.res.AssetManager",
            sdk_version,
            "AssetManager.init: recorded the initial resource configuration"
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn asset_manager_set_apk_assets<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    _apk_assets: JObject<'local>,
    invalidate_caches: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {

        tracing::debug!(
            target: "android.content.res.AssetManager",
            invalidate_caches,
            "AssetManager.native_setApkAssets: GTK-free no-op (asset table deferred; mObject stays 0)"
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn asset_manager_set_configuration<'local>(
    mut env: EnvUnowned<'local>,
    this: JObject<'local>,
    mcc: jint,
    mnc: jint,
    locale: JString<'local>,
    orientation: jint,
    touchscreen: jint,
    density: jint,
    keyboard: jint,
    keyboard_hidden: jint,
    navigation: jint,
    screen_width: jint,
    screen_height: jint,
    smallest_screen_width_dp: jint,
    screen_width_dp: jint,
    screen_height_dp: jint,
    screen_layout: jint,
    ui_mode: jint,
    major_version: jint,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let locale = if locale.is_null() {
            None
        } else {
            Some(locale.try_to_string(env)?)
        };
        let config = asset_configuration::JavaConfiguration {
            mcc,
            mnc,
            locale: locale.as_deref(),
            orientation,
            touchscreen,
            density,
            keyboard,
            keyboard_hidden,
            navigation,
            screen_width,
            screen_height,
            smallest_screen_width_dp,
            screen_width_dp,
            screen_height_dp,
            screen_layout,
            ui_mode,
            sdk_version: major_version,
        }
        .to_res_config();
        asset_configuration::record(env, &this, config)?;
        tracing::debug!(
            target: "android.content.res.AssetManager",
            locale = ?locale,
            density,
            screen_width_dp,
            screen_height_dp,
            smallest_screen_width_dp,
            ui_mode,
            sdk_version = major_version,
            "AssetManager.setConfiguration: recorded the resource configuration"
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn asset_manager_open_xml_asset<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    cookie: jint,
    file_name: JString<'local>,
) -> jlong {
    env.with_env(|env| -> jni::errors::Result<jlong> {
        if file_name.is_null() {
            return Ok(0);
        }
        let name = file_name.try_to_string(env)?;
        match open_xml_block(cookie, &name) {
            Ok(handle) => {
                tracing::debug!(
                    target: "android.content.res.AssetManager",
                    cookie,
                    asset = %name,
                    handle,
                    "AssetManager.openXmlAssetNative: parsed XML asset from APK (Eclipse axml)"
                );
                Ok(handle)
            }
            Err(e) => {
                tracing::warn!(
                    target: "android.content.res.AssetManager",
                    cookie,
                    asset = %name,
                    error = %e,
                    "AssetManager.openXmlAssetNative: could not open XML asset → 0 (FileNotFound)"
                );
                Ok(0)
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn asset_manager_retrieve_attributes<'local>(
    mut env: EnvUnowned<'local>,
    this: JObject<'local>,
    parse_state: jlong,
    attrs: JIntArray<'local>,
    parser: jint,
    out_values: jlong,
    out_indices: jlong,
) -> jboolean {
    env.with_env(|env| -> jni::errors::Result<jboolean> {
        if attrs.is_null() {
            return Ok(false);
        }
        let n = attrs.len(env)?;
        if n == 0 {
            fill_typed_array(out_values, out_indices, &[]);
            return Ok(false);
        }

        let mut ids = vec![0i32; n];
        let start = jint::try_from(0).unwrap_or(0);
        attrs.get_region(env, start, &mut ids)?;

        let config = asset_configuration::of(env, &this);
        let entries = resolve_xml_attributes(parse_state, &ids, &config);
        let changed = entries.iter().filter(|e| e.is_some()).count();

        fill_typed_array(out_values, out_indices, &entries);

        tracing::debug!(
            target: "android.content.res.AssetManager",
            parse_state,
            parser,
            attrs = n,
            changed,
            out_values_null = (out_values == 0),
            out_indices_null = (out_indices == 0),
            "AssetManager.retrieveAttributes: resolved manifest XML attributes by resource id"
        );

        Ok(changed > 0)
    })
    .resolve::<LogErrorAndDefault>()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TypedEntry {
    value_type: i32,

    data: i32,

    resource_id: i32,

    asset_cookie: i32,
}

fn resolve_xml_attributes(
    parse_state: jlong,
    ids: &[i32],
    config: &ResConfig,
) -> Vec<Option<TypedEntry>> {
    xml_registry::with_block(parse_state, |block| {
        let element = block.current_element();
        ids.iter()
            .map(|&id| {
                let element = element?;

                let id_u32 = u32::from_ne_bytes(id.to_ne_bytes());
                let attr = element
                    .attributes
                    .iter()
                    .find(|a| a.name_resource != 0 && a.name_resource == id_u32)?;
                Some(resolve_inline_attr_value(
                    attr.value_type,
                    attr.value_data,
                    config,
                ))
            })
            .collect()
    })
    .unwrap_or_else(|_| vec![None; ids.len()])
}

fn resolve_inline_attr_value(value_type: u8, value_data: u32, config: &ResConfig) -> TypedEntry {
    let mut cur_type = value_type;
    let mut cur_data = value_data;

    let resource_id = if value_type == TYPE_REFERENCE || value_type == TYPE_ATTRIBUTE {
        u32_to_i32(value_data)
    } else {
        0
    };

    let mut string_pool_cookie = XML_BLOCK_COOKIE;
    for _ in 0..MAX_ATTR_RESOLVE_DEPTH {
        if cur_type != TYPE_REFERENCE || cur_data == 0 {
            break;
        }
        match resolve_res_value(cur_data, config) {
            Some(v) => {
                string_pool_cookie = arsc_cookie_for(cur_data);
                cur_type = u8::try_from(v.type_).unwrap_or(0);
                cur_data = u32::from_ne_bytes(v.data.to_ne_bytes());
            }

            None => break,
        }
    }

    let asset_cookie = if cur_type == TYPE_STRING {
        string_pool_cookie
    } else {
        0
    };
    TypedEntry {
        value_type: i32::from(cur_type),
        data: u32_to_i32(cur_data),
        resource_id,
        asset_cookie,
    }
}

fn u32_to_i32(v: u32) -> i32 {
    i32::from_ne_bytes(v.to_ne_bytes())
}

const MAX_THEME_PARENT_DEPTH: usize = 64;

fn merge_theme_style(
    out: &mut std::collections::HashMap<i32, theme_registry::ThemeAttr>,
    style_res: u32,
    config: &ResConfig,
) -> usize {
    let mut contributed = 0usize;
    let mut current = style_res;
    let mut visited = std::collections::HashSet::new();
    for _ in 0..MAX_THEME_PARENT_DEPTH {
        if current == 0 || !visited.insert(current) {
            break;
        }
        let Some(bytes) = arsc_bytes_for(current) else {
            break;
        };
        let Ok(table) = crate::apk::arsc::parse_arsc(bytes) else {
            break;
        };
        let Some(style) = table.resolve_style(current, config) else {
            break;
        };
        for entry in &style.entries {
            if entry.attr_id == 0 {
                continue;
            }
            let key = u32_to_i32(entry.attr_id);

            out.entry(key).or_insert_with(|| {
                contributed += 1;
                theme_registry::ThemeAttr {
                    type_: entry.type_,
                    data: entry.data,

                    source_package: (current >> 24) as u8,
                }
            });
        }
        current = style.parent_id;
    }
    contributed
}

const MAX_ATTR_RESOLVE_DEPTH: usize = 16;

fn resolve_theme_attr(
    attrs: &std::collections::HashMap<i32, theme_registry::ThemeAttr>,
    attr_id: i32,
    config: &ResConfig,
) -> Option<TypedEntry> {
    let mut cur = *attrs.get(&attr_id)?;

    let mut resource_id = if cur.type_ == TYPE_REFERENCE {
        u32_to_i32(cur.data)
    } else {
        0
    };

    let mut string_pool_cookie = arsc_cookie_for_package(cur.source_package);
    for _ in 0..MAX_ATTR_RESOLVE_DEPTH {
        match cur.type_ {
            TYPE_ATTRIBUTE => {
                let next_id = u32_to_i32(cur.data);
                cur = *attrs.get(&next_id)?;
                string_pool_cookie = arsc_cookie_for_package(cur.source_package);
                if cur.type_ == TYPE_REFERENCE {
                    resource_id = u32_to_i32(cur.data);
                }
            }

            TYPE_REFERENCE => {
                if cur.data == 0 {
                    break;
                }
                match resolve_res_value(cur.data, config) {
                    Some(v) => {
                        string_pool_cookie = arsc_cookie_for(cur.data);
                        cur = theme_registry::ThemeAttr {
                            type_: u8::try_from(v.type_).unwrap_or(0),
                            data: u32::from_ne_bytes(v.data.to_ne_bytes()),
                            source_package: (cur.data >> 24) as u8,
                        };

                        if cur.type_ == TYPE_REFERENCE {
                            resource_id = v.data;
                        }
                    }

                    None => break,
                }
            }

            _ => break,
        }
    }
    let asset_cookie = if cur.type_ == TYPE_STRING {
        string_pool_cookie
    } else {
        0
    };
    Some(TypedEntry {
        value_type: i32::from(cur.type_),
        data: u32_to_i32(cur.data),
        resource_id,
        asset_cookie,
    })
}

fn resolve_theme_attributes(
    theme: jlong,
    ids: &[i32],
    config: &ResConfig,
) -> Vec<Option<TypedEntry>> {
    theme_registry::with_theme(theme, |t| {
        ids.iter()
            .map(|&id| resolve_theme_attr(&t.attrs, id, config))
            .collect()
    })
    .unwrap_or_else(|_| vec![None; ids.len()])
}

fn resolve_inline_theme_refs(theme: jlong, entries: &mut [Option<TypedEntry>], config: &ResConfig) {
    let _ = theme_registry::with_theme(theme, |t| {
        for slot in entries.iter_mut() {
            if let Some(entry) = slot {
                if entry.value_type == i32::from(TYPE_ATTRIBUTE) {
                    if let Some(resolved) = resolve_theme_attr(&t.attrs, entry.data, config) {
                        *slot = Some(resolved);
                    }
                }
            }
        }
    });
}

fn typed_array_window(entry: Option<&TypedEntry>) -> [i32; STYLE_NUM_ENTRIES] {
    let mut window = [0; STYLE_NUM_ENTRIES];
    match entry {
        Some(e) => {
            window[STYLE_TYPE] = e.value_type;
            window[STYLE_DATA] = e.data;
            window[STYLE_ASSET_COOKIE] = e.asset_cookie;
            window[STYLE_RESOURCE_ID] = e.resource_id;
        }
        None => {
            window[STYLE_TYPE] = TYPE_NULL;
            window[STYLE_DATA] = DATA_NULL_UNDEFINED;
            window[STYLE_ASSET_COOKIE] = NO_ASSET_COOKIE;
        }
    }
    window
}

fn fill_typed_array(out_values: jlong, out_indices: jlong, entries: &[Option<TypedEntry>]) {
    if out_values != 0 {
        let base = out_values as usize as *mut i32;
        for (attr, entry) in entries.iter().enumerate() {
            let window = typed_array_window(entry.as_ref());
            unsafe {
                std::ptr::copy_nonoverlapping(
                    window.as_ptr(),
                    base.add(attr * STYLE_NUM_ENTRIES),
                    STYLE_NUM_ENTRIES,
                );
            }
        }
    }

    if out_indices != 0 {
        let base = out_indices as usize as *mut i32;

        let mut count: i32 = 0;
        for (attr, entry) in entries.iter().enumerate() {
            if entry.is_some() {
                count += 1;

                let pos = i32::try_from(attr).unwrap_or(i32::MAX);
                unsafe { base.add(count as usize).write(pos) };
            }
        }

        unsafe { base.write(count) };
    }
}

extern "system" fn asset_manager_new_theme<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        match theme_registry::allocate() {
            Ok(handle) => {
                tracing::debug!(
                    target: "android.content.res.AssetManager",
                    handle,
                    "AssetManager.newTheme: allocated non-GTK theme-registry handle"
                );
                Ok(handle)
            }
            Err(e) => {
                tracing::warn!(
                    target: "android.content.res.AssetManager",
                    error = %e,
                    "AssetManager.newTheme: theme-registry allocate failed → 0 (no theme)"
                );
                Ok(0)
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn asset_manager_delete_theme<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    theme: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if theme == 0 {
            return Ok(());
        }
        if let Err(e) = theme_registry::free(theme) {
            tracing::error!(
                target: "android.content.res.AssetManager",
                theme,
                error = %e,
                "AssetManager.deleteTheme: the theme handle was not live"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn asset_manager_apply_theme_style<'local>(
    mut env: EnvUnowned<'local>,
    this: JObject<'local>,
    theme: jlong,
    style_res: jint,
    force: jboolean,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let config = asset_configuration::of(env, &this);
        let mut chain = std::collections::HashMap::new();
        let style_u32 = u32::from_ne_bytes(style_res.to_ne_bytes());
        let resolved = merge_theme_style(&mut chain, style_u32, &config);

        let merged = theme_registry::with_theme(theme, |t| {
            t.styles.push(style_res);

            for (attr, val) in &chain {
                if force {
                    t.attrs.insert(*attr, *val);
                } else {
                    t.attrs.entry(*attr).or_insert(*val);
                }
            }
            t.attrs.len()
        });
        match merged {
            Ok(total) => tracing::debug!(
                target: "android.content.res.AssetManager",
                theme,
                style_res = format_args!("0x{style_u32:08x}"),
                force,
                resolved,
                total,
                "AssetManager.applyThemeStyle: merged style + parent chain into non-GTK theme"
            ),
            Err(e) => tracing::debug!(
                target: "android.content.res.AssetManager",
                theme,
                style_res = format_args!("0x{style_u32:08x}"),
                error = %e,
                "AssetManager.applyThemeStyle: invalid theme handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn asset_manager_copy_theme<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    dest: jlong,
    source: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        let src = theme_registry::with_theme(source, |t| (t.styles.clone(), t.attrs.clone()));
        match src {
            Ok((styles, attrs)) => {
                if let Err(e) = theme_registry::with_theme(dest, |t| {
                    t.styles = styles;
                    t.attrs = attrs;
                }) {
                    tracing::debug!(
                        target: "android.content.res.AssetManager",
                        dest,
                        source,
                        error = %e,
                        "AssetManager.copyTheme: invalid dest theme handle (ignored)"
                    );
                } else {
                    tracing::debug!(
                        target: "android.content.res.AssetManager",
                        dest,
                        source,
                        "AssetManager.copyTheme: copied non-GTK theme styles"
                    );
                }
            }
            Err(e) => tracing::debug!(
                target: "android.content.res.AssetManager",
                dest,
                source,
                error = %e,
                "AssetManager.copyTheme: invalid source theme handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn asset_manager_apply_style<'local>(
    mut env: EnvUnowned<'local>,
    this: JObject<'local>,
    theme: jlong,
    parser: jlong,
    _def_style_attr: jint,
    _def_style_res: jint,
    attrs: JIntArray<'local>,
    _length: jint,
    out_values: jlong,
    out_indices: jlong,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let n = if attrs.is_null() { 0 } else { attrs.len(env)? };
        let mut entries = vec![None; n];

        if n != 0 {
            let config = asset_configuration::of(env, &this);
            let mut ids = vec![0i32; n];
            attrs.get_region(env, 0, &mut ids)?;
            if parser != 0 {
                entries = resolve_xml_attributes(parser, &ids, &config);

                resolve_inline_theme_refs(theme, &mut entries, &config);
            }

            let theme_entries = resolve_theme_attributes(theme, &ids, &config);
            for (slot, theme_entry) in entries.iter_mut().zip(theme_entries) {
                if slot.is_none() {
                    *slot = theme_entry;
                }
            }
        }
        let changed = entries.iter().filter(|e| e.is_some()).count();

        fill_typed_array(out_values, out_indices, &entries);
        tracing::debug!(
            target: "android.content.res.AssetManager",
            theme,
            parser,
            attrs = n,
            changed,
            "AssetManager.applyStyle: resolved styled attributes (XML element + theme, non-GTK)"
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn asset_manager_get_resource_name<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    resid: jint,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        let resid_u32 = u32::from_ne_bytes(resid.to_ne_bytes());
        match resolve_resource_name(resid_u32) {
            Some(name) => {
                tracing::debug!(
                    target: "android.content.res.AssetManager",
                    resid = format_args!("0x{resid_u32:08x}"),
                    name = %name,
                    "AssetManager.getResourceName: resolved via resources.arsc"
                );
                env.new_string(name)
            }
            None => {
                tracing::warn!(
                    target: "android.content.res.AssetManager",
                    resid = format_args!("0x{resid_u32:08x}"),
                    "AssetManager.getResourceName: id not in resources.arsc → null (NotFoundException)"
                );
                Ok(JString::default())
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn asset_manager_get_resource_package_name<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    resid: jint,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        let resid_u32 = u32::from_ne_bytes(resid.to_ne_bytes());
        match resolve_resource_package_name(resid_u32) {
            Some(pkg) => {
                tracing::debug!(
                    target: "android.content.res.AssetManager",
                    resid = format_args!("0x{resid_u32:08x}"),
                    package = %pkg,
                    "AssetManager.getResourcePackageName: resolved via resources.arsc"
                );
                env.new_string(pkg)
            }
            None => {
                tracing::warn!(
                    target: "android.content.res.AssetManager",
                    resid = format_args!("0x{resid_u32:08x}"),
                    "AssetManager.getResourcePackageName: id not in resources.arsc → null (NotFoundException)"
                );
                Ok(JString::default())
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

fn resolve_resource_package_name(resid: u32) -> Option<String> {
    let bytes = arsc_bytes_for(resid)?;
    let table = crate::apk::arsc::parse_arsc(bytes).ok()?;
    let package_id = (resid >> 24) as u8;
    table.package_name(package_id).map(str::to_owned)
}

extern "system" fn asset_manager_get_resource_identifier<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    name: JString<'local>,
    def_type: JString<'local>,
    def_package: JString<'local>,
) -> jint {
    env.with_env(|env| -> jni::errors::Result<jint> {
        if name.is_null() {
            return Ok(0);
        }
        let name = name.try_to_string(env)?;
        let read_opt = |s: &JString<'local>| -> jni::errors::Result<String> {
            if s.is_null() {
                Ok(String::new())
            } else {
                s.try_to_string(env)
            }
        };
        let def_type = read_opt(&def_type)?;
        let def_package = read_opt(&def_package)?;
        let resid = resolve_resource_identifier(&name, &def_type, &def_package);
        tracing::debug!(
            target: "android.content.res.AssetManager",
            name = %name,
            def_type = %def_type,
            def_package = %def_package,
            resid = format_args!("0x{resid:08x}"),
            "AssetManager.getResourceIdentifier"
        );

        Ok(i32::from_ne_bytes(resid.to_ne_bytes()))
    })
    .resolve::<LogErrorAndDefault>()
}

fn resolve_resource_identifier(name: &str, def_type: &str, def_package: &str) -> u32 {
    let (pkg_in_name, rest) = match name.split_once(':') {
        Some((p, r)) => (Some(p), r),
        None => (None, name),
    };
    let (type_in_name, entry) = match rest.split_once('/') {
        Some((t, e)) => (Some(t), e),
        None => (None, rest),
    };
    let entry = entry.trim();
    if entry.is_empty() {
        return 0;
    }
    let pick = |from_name: Option<&str>, default: &str| -> Option<String> {
        from_name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                let d = default.trim();
                (!d.is_empty()).then(|| d.to_owned())
            })
    };
    let pkg = pick(pkg_in_name, def_package);
    let Some(typ) = pick(type_in_name, def_type) else {
        return 0;
    };

    let probe_id: u32 = if pkg.as_deref() == Some("android") {
        0x0100_0000
    } else {
        0x7f00_0000
    };
    let Some(bytes) = arsc_bytes_for(probe_id) else {
        return 0;
    };
    let Ok(table) = crate::apk::arsc::parse_arsc(bytes) else {
        return 0;
    };
    table
        .find_resource_id(pkg.as_deref(), &typ, entry)
        .unwrap_or(0)
}

fn read_asset_bytes(name: &str) -> Option<Vec<u8>> {
    read_asset_bytes_from(APK_PATH.get()?, name)
}

fn read_asset_bytes_from(apk_path: &str, name: &str) -> Option<Vec<u8>> {
    let mut apk = crate::apk::cache::APP_APK
        .open(std::path::Path::new(apk_path))
        .ok()?;
    asset_entry_candidates(name).find_map(|entry| apk.read_entry(&entry).ok())
}

fn asset_entry_candidates(name: &str) -> impl Iterator<Item = String> {
    let fallback = (!name.starts_with("assets/")).then(|| format!("assets/{name}"));
    std::iter::once(name.to_owned()).chain(fallback)
}

extern "system" fn asset_manager_open_asset<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    file_name: JString<'local>,
    _access_mode: jint,
) -> jlong {
    env.with_env(|env| -> jni::errors::Result<jlong> {
        if file_name.is_null() {
            return Ok(0);
        }
        let name = file_name.try_to_string(env)?;
        let Some(bytes) = read_asset_bytes(&name) else {
            tracing::warn!(
                target: "android.content.res.AssetManager",
                asset = %name,
                "AssetManager.openAsset: assets/<name> not found → 0 (FileNotFoundException)"
            );
            return Ok(0);
        };
        match asset_registry::store(bytes) {
            Ok(handle) => {
                tracing::debug!(
                    target: "android.content.res.AssetManager",
                    asset = %name,
                    "AssetManager.openAsset: opened via src/apk"
                );
                Ok(handle)
            }
            Err(e) => {
                tracing::warn!(
                    target: "android.content.res.AssetManager",
                    asset = %name, error = %e,
                    "AssetManager.openAsset: registry store failed → 0"
                );
                Ok(0)
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

#[derive(Debug)]
enum AssetFdError {
    Apk(crate::apk::ApkError),

    Compressed,

    Io(std::io::Error),
}

impl fmt::Display for AssetFdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Apk(e) => write!(f, "apk: {e}"),
            Self::Compressed => f.write_str("entry is compressed (fd path serves Stored only)"),
            Self::Io(e) => write!(f, "open APK for fd: {e}"),
        }
    }
}

fn asset_fd_for(apk_path: &str, name: &str) -> Result<(c_int, u64, u64), AssetFdError> {
    let mut apk = crate::apk::cache::APP_APK
        .open(std::path::Path::new(apk_path))
        .map_err(AssetFdError::Apk)?;
    let mut resolved = Err(AssetFdError::Apk(crate::apk::ApkError::EntryMissing(
        name.to_owned(),
    )));
    for entry in asset_entry_candidates(name) {
        match apk.entry_span(&entry) {
            Ok(span) => {
                resolved = Ok(span);
                break;
            }
            Err(e) => resolved = Err(AssetFdError::Apk(e)),
        }
    }
    let span = resolved?;
    if !span.stored {
        return Err(AssetFdError::Compressed);
    }
    let file = crate::apk::reopen(apk.file()).map_err(AssetFdError::Io)?;
    Ok((
        std::os::fd::IntoRawFd::into_raw_fd(file),
        span.data_start,
        span.uncompressed_size,
    ))
}

fn write_long_out_param(env: &mut Env, arr: &JLongArray, value: jlong, which: &str) -> bool {
    if arr.is_null() {
        return true;
    }
    match arr.set_region(env, 0, &[value]) {
        Ok(()) => true,
        Err(e) => {
            if env.exception_check() {
                env.exception_describe();
                env.exception_clear();
            }
            tracing::warn!(
                target: "android.content.res.AssetManager",
                which,
                error = %e,
                "AssetManager.openAssetFd: out-param write failed"
            );
            false
        }
    }
}

extern "system" fn asset_manager_open_asset_fd<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    file_name: JString<'local>,
    _mode: jint,
    out_offsets: JLongArray<'local>,
    out_lengths: JLongArray<'local>,
) -> jint {
    env.with_env(|env| -> jni::errors::Result<jint> {
        if file_name.is_null() {
            return Ok(-1);
        }
        let name = match file_name.try_to_string(env) {
            Ok(n) => n,
            Err(e) => {
                if env.exception_check() {
                    env.exception_describe();
                    env.exception_clear();
                }
                tracing::warn!(
                    target: "android.content.res.AssetManager",
                    error = %e,
                    "AssetManager.openAssetFd: could not read fileName → -1"
                );
                return Ok(-1);
            }
        };
        let Some(apk_path) = APK_PATH.get() else {
            tracing::warn!(
                target: "android.content.res.AssetManager",
                asset = %name,
                "AssetManager.openAssetFd: APK path unset → -1 (FileNotFoundException)"
            );
            return Ok(-1);
        };
        let (fd, offset, length) = match asset_fd_for(apk_path, &name) {
            Ok(triple) => triple,
            Err(e) => {
                tracing::info!(
                    target: "android.content.res.AssetManager",
                    asset = %name,
                    reason = %e,
                    "AssetManager.openAssetFd: not fd-servable → -1 (FileNotFoundException)"
                );
                return Ok(-1);
            }
        };

        let (Ok(offset), Ok(length)) = (jlong::try_from(offset), jlong::try_from(length)) else {
            unsafe { libc::close(fd) };
            return Ok(-1);
        };
        if !write_long_out_param(env, &out_offsets, offset, "outOffsets")
            || !write_long_out_param(env, &out_lengths, length, "outLengths")
        {
            unsafe { libc::close(fd) };
            return Ok(-1);
        }
        tracing::debug!(
            target: "android.content.res.AssetManager",
            asset = %name,
            fd,
            offset,
            length,
            "AssetManager.openAssetFd: served Stored entry by fd (ownership → Java)"
        );
        Ok(fd)
    })
    .resolve::<LogErrorAndDefault>()
}

fn atl_read_asset_return(outcome: &Result<Vec<u8>, asset_registry::AssetRegistryError>) -> jint {
    match outcome {
        Ok(read) => i32::try_from(read.len()).unwrap_or(jint::MAX),
        Err(_) => -1,
    }
}

extern "system" fn asset_manager_read_asset<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    asset: jlong,
    b: JByteArray<'local>,
    off: jlong,
    len: jlong,
) -> jint {
    env.with_env(|env| -> jni::errors::Result<jint> {
        if len <= 0 {
            return Ok(0);
        }

        let array_len = i64::try_from(b.len(env).unwrap_or(0)).unwrap_or(i64::MAX);
        let off = off.clamp(0, array_len);
        let fits = (array_len - off).max(0);
        let want = usize::try_from(len.min(fits)).unwrap_or(0);
        if want == 0 {
            return Ok(0);
        }
        let outcome = asset_registry::with_stream(asset, |s| {
            let mut tmp = vec![0u8; want];
            let n = s.read(&mut tmp);
            tmp.truncate(n);
            tmp
        });

        let ret = atl_read_asset_return(&outcome);
        let Ok(read) = outcome else {
            return Ok(ret);
        };
        if !read.is_empty() {
            let signed: Vec<i8> = read.iter().map(|&x| i8::from_ne_bytes([x])).collect();
            let start = jni::sys::jsize::try_from(off).unwrap_or(jni::sys::jsize::MAX);
            b.set_region(env, start, &signed)?;
        }
        tracing::debug!(
            target: "android.content.res.AssetManager",
            asset, off, len, returned = ret,
            "AssetManager.readAsset"
        );
        Ok(ret)
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn asset_manager_read_asset_char<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    asset: jlong,
) -> jint {
    env.with_env(|_env| -> jni::errors::Result<jint> {
        let byte = asset_registry::with_stream(asset, |s| {
            let mut one = [0u8; 1];
            (s.read(&mut one) == 1).then_some(one[0])
        });
        Ok(match byte {
            Ok(Some(b)) => jint::from(b),
            Ok(None) | Err(_) => -1,
        })
    })
    .resolve::<LogErrorAndDefault>()
}

fn atl_seek_whence_to_lseek(whence: jint) -> i32 {
    match whence {
        w if w < 0 => 0,
        0 => 1,
        _ => 2,
    }
}

extern "system" fn asset_manager_seek_asset<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    asset: jlong,
    offset: jlong,
    whence: jint,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        Ok(
            asset_registry::with_stream(asset, |s| {
                s.seek(offset, atl_seek_whence_to_lseek(whence))
            })
            .unwrap_or(-1),
        )
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn asset_manager_get_asset_length<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    asset: jlong,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        Ok(
            asset_registry::with_stream(asset, |s| i64::try_from(s.len()).unwrap_or(i64::MAX))
                .unwrap_or(-1),
        )
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn asset_manager_get_asset_remaining_length<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    asset: jlong,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        Ok(
            asset_registry::with_stream(asset, |s| {
                i64::try_from(s.remaining()).unwrap_or(i64::MAX)
            })
            .unwrap_or(-1),
        )
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn asset_manager_destroy_asset<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    asset: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        let _ = asset_registry::free(asset);
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

fn register_asset_stream_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let natives: [(&JNIStr, &JNIStr, *mut c_void); 7] = [
        (
            ASSET_MANAGER_READ_ASSET_NAME,
            ASSET_MANAGER_READ_ASSET_SIG,
            asset_manager_read_asset as *mut c_void,
        ),
        (
            ASSET_MANAGER_READ_ASSET_CHAR_NAME,
            ASSET_MANAGER_READ_ASSET_CHAR_SIG,
            asset_manager_read_asset_char as *mut c_void,
        ),
        (
            ASSET_MANAGER_OPEN_ASSET_FD_NAME,
            ASSET_MANAGER_OPEN_ASSET_FD_SIG,
            asset_manager_open_asset_fd as *mut c_void,
        ),
        (
            ASSET_MANAGER_SEEK_ASSET_NAME,
            ASSET_MANAGER_SEEK_ASSET_SIG,
            asset_manager_seek_asset as *mut c_void,
        ),
        (
            ASSET_MANAGER_GET_ASSET_LENGTH_NAME,
            ASSET_MANAGER_GET_ASSET_LENGTH_SIG,
            asset_manager_get_asset_length as *mut c_void,
        ),
        (
            ASSET_MANAGER_GET_ASSET_REMAINING_LENGTH_NAME,
            ASSET_MANAGER_GET_ASSET_REMAINING_LENGTH_SIG,
            asset_manager_get_asset_remaining_length as *mut c_void,
        ),
        (
            ASSET_MANAGER_DESTROY_ASSET_NAME,
            ASSET_MANAGER_DESTROY_ASSET_SIG,
            asset_manager_destroy_asset as *mut c_void,
        ),
    ];
    let mut bound = 0u32;
    for (name, sig, ptr) in natives {
        let class = env.find_class(ASSET_MANAGER_CLASS)?;

        let method = unsafe { NativeMethod::from_raw_parts(name, sig, ptr) };
        match unsafe { env.register_native_methods(&class, std::slice::from_ref(&method)) } {
            Ok(()) => bound += 1,
            Err(_) => {
                if env.exception_check() {
                    env.exception_clear();
                }
                tracing::debug!(
                    class = "android/content/res/AssetManager",
                    method = %name.to_str(),
                    "asset-stream native not declared on this ATL build (skipped)"
                );
            }
        }
    }
    tracing::info!(
        class = "android/content/res/AssetManager",
        bound,
        "registered Eclipse's non-GTK asset-stream natives (per-native best-effort: readAsset/readAssetChar/openAssetFd/seekAsset/getAssetLength/getAssetRemainingLength/destroyAsset)"
    );
    Ok(())
}

static APP_ARSC: OnceLock<Option<Vec<u8>>> = OnceLock::new();
static FRAMEWORK_ARSC: OnceLock<Option<Vec<u8>>> = OnceLock::new();

fn cached_arsc_bytes(
    cache: &OnceLock<Option<Vec<u8>>>,
    load: impl FnOnce() -> Option<Vec<u8>>,
) -> Option<&[u8]> {
    cache.get_or_init(load).as_deref()
}

fn framework_arsc_bytes<'a>(
    cache: &'a OnceLock<Option<Vec<u8>>>,
    framework_res_apk: &std::path::Path,
) -> Option<&'a [u8]> {
    cached_arsc_bytes(cache, || {
        let mut apk = crate::apk::cache::FRAMEWORK_RES_APK
            .open(framework_res_apk)
            .ok()?;
        apk.read_entry("resources.arsc").ok()
    })
}

fn arsc_bytes_for(resid: u32) -> Option<&'static [u8]> {
    if (resid >> 24) as u8 == 0x01 {
        let fw = crate::runtime::find_framework().ok()?;
        framework_arsc_bytes(&FRAMEWORK_ARSC, &fw.framework_res_apk)
    } else {
        let apk_path = APK_PATH.get()?;
        cached_arsc_bytes(&APP_ARSC, || {
            let mut apk = crate::apk::cache::APP_APK
                .open(std::path::Path::new(apk_path))
                .ok()?;
            apk.read_entry("resources.arsc").ok()
        })
    }
}

fn resolve_resource_name(resid: u32) -> Option<String> {
    let bytes = arsc_bytes_for(resid)?;
    let table = crate::apk::arsc::parse_arsc(bytes).ok()?;

    let package_id = (resid >> 24) as u8;
    let type_id = ((resid >> 16) & 0xff) as u8;
    let key_index = table.key_index(resid)?;
    let type_name = table.type_name(package_id, type_id).ok().flatten()?;
    let entry_name = table.key_name(package_id, key_index).ok().flatten()?;

    match table.package_name(package_id) {
        Some(pkg) => Some(format!("{pkg}:{type_name}/{entry_name}")),
        None => Some(format!("{type_name}/{entry_name}")),
    }
}

struct ResolvedResValue {
    type_: i32,
    data: i32,
    density: u16,
    string: Option<String>,
}

fn resolve_res_value(resid: u32, config: &ResConfig) -> Option<ResolvedResValue> {
    let bytes = arsc_bytes_for(resid)?;
    let table = crate::apk::arsc::parse_arsc(bytes).ok()?;
    let resolved = table.resource_value(resid, config)?;

    if resolved.is_complex {
        return None;
    }
    let string = if resolved.type_ == RES_VALUE_TYPE_STRING {
        table.value_string(resolved.data).ok().flatten()
    } else {
        None
    };
    Some(ResolvedResValue {
        type_: i32::from(resolved.type_),
        data: u32_to_i32(resolved.data),
        density: resolved.density,
        string,
    })
}

extern "system" fn asset_manager_load_resource_value<'local>(
    mut env: EnvUnowned<'local>,
    this: JObject<'local>,
    resid: jint,
    density: jshort,
    out_value: JObject<'local>,
    _resolve_refs: jboolean,
) -> jint {
    env.with_env(|env| -> jni::errors::Result<jint> {
        let resid_u32 = u32::from_ne_bytes(resid.to_ne_bytes());
        let mut config = asset_configuration::of(env, &this);
        if let Ok(density @ 1..) = u16::try_from(density) {
            config.density = density;
        }
        let Some(resolved) = resolve_res_value(resid_u32, &config) else {
            tracing::warn!(
                target: "android.content.res.AssetManager",
                resid = format_args!("0x{resid_u32:08x}"),
                "AssetManager.loadResourceValue: id not in resources.arsc → 0 (not found)"
            );
            return Ok(0);
        };
        if out_value.is_null() {
            return Ok(0);
        }

        let int_sig =
            unsafe { FieldSignature::from_raw_parts(INT_SIG, JavaType::Primitive(Primitive::Int)) };
        env.set_field(
            &out_value,
            jni_str!("type"),
            &int_sig,
            resolved.type_.into(),
        )?;
        env.set_field(&out_value, jni_str!("data"), &int_sig, resolved.data.into())?;

        env.set_field(
            &out_value,
            jni_str!("assetCookie"),
            &int_sig,
            arsc_cookie_for(resid_u32).into(),
        )?;
        env.set_field(&out_value, jni_str!("resourceId"), &int_sig, resid.into())?;
        env.set_field(
            &out_value,
            jni_str!("density"),
            &int_sig,
            jint::from(resolved.density).into(),
        )?;

        if let Some(s) = &resolved.string {
            let jstr = env.new_string(s)?;

            let cs_sig =
                unsafe { FieldSignature::from_raw_parts(CHAR_SEQUENCE_SIG, JavaType::Object) };
            env.set_field(
                &out_value,
                jni_str!("string"),
                &cs_sig,
                JValue::Object(&jstr),
            )?;
        }
        tracing::debug!(
            target: "android.content.res.AssetManager",
            resid = format_args!("0x{resid_u32:08x}"),
            type_ = resolved.type_,
            data = resolved.data,
            string = ?resolved.string,
            "AssetManager.loadResourceValue: wrote TypedValue from resources.arsc"
        );
        Ok(ECLIPSE_ASSET_COOKIE)
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn asset_manager_load_theme_attribute_value<'local>(
    mut env: EnvUnowned<'local>,
    this: JObject<'local>,
    theme: jlong,
    ident: jint,
    out_value: JObject<'local>,
    _resolve_refs: jboolean,
) -> jint {
    env.with_env(|env| -> jni::errors::Result<jint> {
        if out_value.is_null() {

            return Ok(0);
        }

        let config = asset_configuration::of(env, &this);
        let entry =
            theme_registry::with_theme(theme, |t| resolve_theme_attr(&t.attrs, ident, &config))
            .ok()
            .flatten();
        let Some(entry) = entry else {
            tracing::debug!(
                target: "android.content.res.AssetManager",
                theme,
                ident = format_args!("0x{:08x}", u32::from_ne_bytes(ident.to_ne_bytes())),
                "AssetManager.loadThemeAttributeValue: attr not in theme → 0 (not resolved)"
            );
            return Ok(0);
        };

        let int_sig =
            unsafe { FieldSignature::from_raw_parts(INT_SIG, JavaType::Primitive(Primitive::Int)) };
        env.set_field(
            &out_value,
            jni_str!("type"),
            &int_sig,
            entry.value_type.into(),
        )?;
        env.set_field(&out_value, jni_str!("data"), &int_sig, entry.data.into())?;
        env.set_field(
            &out_value,
            jni_str!("assetCookie"),
            &int_sig,
            entry.asset_cookie.into(),
        )?;
        env.set_field(
            &out_value,
            jni_str!("resourceId"),
            &int_sig,
            entry.resource_id.into(),
        )?;

        if entry.value_type == i32::from(TYPE_STRING) {
            if let Some(s) =
                arsc_pool_string(entry.asset_cookie, u32::from_ne_bytes(entry.data.to_ne_bytes()))
            {
                let jstr = env.new_string(&s)?;

                let cs_sig =
                    unsafe { FieldSignature::from_raw_parts(CHAR_SEQUENCE_SIG, JavaType::Object) };
                env.set_field(
                    &out_value,
                    jni_str!("string"),
                    &cs_sig,
                    JValue::Object(&jstr),
                )?;
            } else {
                tracing::warn!(
                    target: "android.content.res.AssetManager",
                    theme,
                    cookie = entry.asset_cookie,
                    index = entry.data,
                    "AssetManager.loadThemeAttributeValue: TYPE_STRING pool index unresolvable (string left null)"
                );
            }
        }
        tracing::debug!(
            target: "android.content.res.AssetManager",
            theme,
            ident = format_args!("0x{:08x}", u32::from_ne_bytes(ident.to_ne_bytes())),
            type_ = entry.value_type,
            data = entry.data,
            resource_id = entry.resource_id,
            "AssetManager.loadThemeAttributeValue: wrote TypedValue from theme attrs"
        );
        Ok(ECLIPSE_ASSET_COOKIE)
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn asset_manager_get_pooled_string<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    block: jint,
    id: jint,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        let index = u32::from_ne_bytes(id.to_ne_bytes());
        match arsc_pool_string(block, index) {
            Some(s) => env.new_string(&s),
            None => {
                tracing::warn!(
                    target: "android.content.res.AssetManager",
                    block,
                    index,
                    "AssetManager.getPooledString: unknown cookie or out-of-range pool index → null"
                );
                Ok(JString::default())
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

fn open_xml_block(cookie: jint, name: &str) -> Result<jlong, AssetError> {
    let bytes = if cookie == ARSC_FRAMEWORK_COOKIE {
        let fw = crate::runtime::find_framework().map_err(|_| AssetError::NoApkPath)?;
        let mut apk = crate::apk::cache::FRAMEWORK_RES_APK.open(&fw.framework_res_apk)?;
        apk.read_entry(name)?
    } else {
        let apk_path = APK_PATH.get().ok_or(AssetError::NoApkPath)?;
        let mut apk = crate::apk::cache::APP_APK.open(std::path::Path::new(apk_path))?;
        apk.read_entry(name)?
    };
    let doc = crate::apk::axml::parse_document(&bytes)?;
    let handle = xml_registry::store(doc)?;
    Ok(handle)
}

#[derive(Debug)]
enum AssetError {
    NoApkPath,

    Apk(crate::apk::ApkError),

    Axml(crate::apk::axml::AxmlError),

    Registry(xml_registry::XmlRegistryError),
}

impl fmt::Display for AssetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoApkPath => f.write_str("no APK path was stashed for asset access"),
            Self::Apk(e) => write!(f, "APK read error: {e}"),
            Self::Axml(e) => write!(f, "binary-XML parse error: {e}"),
            Self::Registry(e) => write!(f, "xml-block registry error: {e}"),
        }
    }
}

impl From<crate::apk::ApkError> for AssetError {
    fn from(e: crate::apk::ApkError) -> Self {
        Self::Apk(e)
    }
}
impl From<crate::apk::axml::AxmlError> for AssetError {
    fn from(e: crate::apk::axml::AxmlError) -> Self {
        Self::Axml(e)
    }
}
impl From<xml_registry::XmlRegistryError> for AssetError {
    fn from(e: xml_registry::XmlRegistryError) -> Self {
        Self::Registry(e)
    }
}

fn register_asset_manager_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(ASSET_MANAGER_CLASS)?;
    let methods = [
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_INIT_NAME,
                ASSET_MANAGER_INIT_SIG,
                asset_manager_init as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_SET_APK_ASSETS_NAME,
                ASSET_MANAGER_SET_APK_ASSETS_SIG,
                asset_manager_set_apk_assets as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_SET_CONFIGURATION_NAME,
                ASSET_MANAGER_SET_CONFIGURATION_SIG,
                asset_manager_set_configuration as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_OPEN_XML_ASSET_NAME,
                ASSET_MANAGER_OPEN_XML_ASSET_SIG,
                asset_manager_open_xml_asset as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_RETRIEVE_ATTRIBUTES_NAME,
                ASSET_MANAGER_RETRIEVE_ATTRIBUTES_SIG,
                asset_manager_retrieve_attributes as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_NEW_THEME_NAME,
                ASSET_MANAGER_NEW_THEME_SIG,
                asset_manager_new_theme as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_DELETE_THEME_NAME,
                ASSET_MANAGER_DELETE_THEME_SIG,
                asset_manager_delete_theme as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_APPLY_THEME_STYLE_NAME,
                ASSET_MANAGER_APPLY_THEME_STYLE_SIG,
                asset_manager_apply_theme_style as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_COPY_THEME_NAME,
                ASSET_MANAGER_COPY_THEME_SIG,
                asset_manager_copy_theme as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_APPLY_STYLE_NAME,
                ASSET_MANAGER_APPLY_STYLE_SIG,
                asset_manager_apply_style as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_GET_RESOURCE_NAME_NAME,
                ASSET_MANAGER_GET_RESOURCE_NAME_SIG,
                asset_manager_get_resource_name as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_GET_RESOURCE_PACKAGE_NAME_NAME,
                ASSET_MANAGER_GET_RESOURCE_PACKAGE_NAME_SIG,
                asset_manager_get_resource_package_name as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_GET_RESOURCE_IDENTIFIER_NAME,
                ASSET_MANAGER_GET_RESOURCE_IDENTIFIER_SIG,
                asset_manager_get_resource_identifier as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_OPEN_ASSET_NAME,
                ASSET_MANAGER_OPEN_ASSET_SIG,
                asset_manager_open_asset as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_LOAD_RESOURCE_VALUE_NAME,
                ASSET_MANAGER_LOAD_RESOURCE_VALUE_SIG,
                asset_manager_load_resource_value as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_LOAD_THEME_ATTRIBUTE_VALUE_NAME,
                ASSET_MANAGER_LOAD_THEME_ATTRIBUTE_VALUE_SIG,
                asset_manager_load_theme_attribute_value as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_GET_POOLED_STRING_NAME,
                ASSET_MANAGER_GET_POOLED_STRING_SIG,
                asset_manager_get_pooled_string as *mut std::ffi::c_void,
            )
        },
    ];

    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/content/res/AssetManager",
        "registered Eclipse's non-GTK backing for AssetManager.init + native_setApkAssets + setConfiguration + openXmlAssetNative + retrieveAttributes + newTheme + deleteTheme + applyThemeStyle + copyTheme + applyStyle + getResourceName + getResourcePackageName + getResourceIdentifier + openAsset + loadResourceValue + loadThemeAttributeValue"
    );
    Ok(())
}

pub const XML_BLOCK_CLASS: &JNIStr = jni_str!("android/content/res/XmlBlock");

const XML_BLOCK_CREATE_PARSE_STATE_NAME: &JNIStr = jni_str!("nativeCreateParseState");
const XML_BLOCK_CREATE_PARSE_STATE_SIG: &JNIStr = jni_str!("(J)J");

const XML_BLOCK_NEXT_NAME: &JNIStr = jni_str!("nativeNext");
const XML_BLOCK_NEXT_SIG: &JNIStr = jni_str!("(J)I");

const XML_BLOCK_DESTROY_PARSE_STATE_NAME: &JNIStr = jni_str!("nativeDestroyParseState");
const XML_BLOCK_DESTROY_PARSE_STATE_SIG: &JNIStr = jni_str!("(J)V");

const XML_BLOCK_GET_NAME_NAME: &JNIStr = jni_str!("nativeGetName");
const XML_BLOCK_GET_NAME_SIG: &JNIStr = jni_str!("(J)Ljava/lang/String;");

const XML_BLOCK_DESTROY_NAME: &JNIStr = jni_str!("nativeDestroy");
const XML_BLOCK_DESTROY_SIG: &JNIStr = jni_str!("(J)V");

const XML_BLOCK_GET_ATTR_INDEX_NAME: &JNIStr = jni_str!("nativeGetAttributeIndex");
const XML_BLOCK_GET_ATTR_INDEX_SIG: &JNIStr = jni_str!("(JLjava/lang/String;Ljava/lang/String;)I");

const XML_ATTR_NOT_FOUND: jint = -1;

const XML_BLOCK_GET_ATTR_STRING_VALUE_NAME: &JNIStr = jni_str!("nativeGetAttributeStringValue");
const XML_BLOCK_GET_ATTR_STRING_VALUE_SIG: &JNIStr = jni_str!("(JI)Ljava/lang/String;");

const XML_BLOCK_GET_ATTR_DATA_TYPE_NAME: &JNIStr = jni_str!("nativeGetAttributeDataType");
const XML_BLOCK_GET_ATTR_DATA_TYPE_SIG: &JNIStr = jni_str!("(JI)I");

const XML_BLOCK_GET_ATTR_COUNT_NAME: &JNIStr = jni_str!("nativeGetAttributeCount");
const XML_BLOCK_GET_ATTR_COUNT_SIG: &JNIStr = jni_str!("(J)I");

const XML_BLOCK_GET_ATTR_RESOURCE_NAME: &JNIStr = jni_str!("nativeGetAttributeResource");
const XML_BLOCK_GET_ATTR_RESOURCE_SIG: &JNIStr = jni_str!("(JI)I");

const XML_BLOCK_GET_ATTR_DATA_NAME: &JNIStr = jni_str!("nativeGetAttributeData");
const XML_BLOCK_GET_ATTR_DATA_SIG: &JNIStr = jni_str!("(JI)I");

const XML_TYPE_NULL: jint = 0x00;

const XML_BLOCK_GET_LINE_NUMBER_NAME: &JNIStr = jni_str!("nativeGetLineNumber");
const XML_BLOCK_GET_LINE_NUMBER_SIG: &JNIStr = jni_str!("(J)I");

const XML_BLOCK_GET_POOLED_STRING_NAME: &JNIStr = jni_str!("nativeGetPooledString");
const XML_BLOCK_GET_POOLED_STRING_SIG: &JNIStr = jni_str!("(JI)Ljava/lang/String;");

const XML_LINE_UNKNOWN: jint = -1;

const XML_EVENT_END_DOCUMENT: jint = 1;
const XML_EVENT_START_TAG: jint = 2;
const XML_EVENT_END_TAG: jint = 3;
const XML_EVENT_TEXT: jint = 4;

extern "system" fn xml_block_create_parse_state<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    block: jlong,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        match xml_registry::with_block(block, |b| {
            b.cursor = 0;
            b.current = None;
        }) {
            Ok(()) => Ok(block),
            Err(e) => {
                tracing::warn!(
                    target: "android.content.res.XmlBlock",
                    block,
                    error = %e,
                    "XmlBlock.nativeCreateParseState: invalid block handle → 0"
                );
                Ok(0)
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn xml_block_next<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    state: jlong,
) -> jint {
    env.with_env(|_env| -> jni::errors::Result<jint> {
        let event = xml_registry::with_block(state, |b| loop {
            match b.next_event() {
                Some(crate::apk::axml::XmlEventKind::StartTag(_)) => break XML_EVENT_START_TAG,
                Some(crate::apk::axml::XmlEventKind::EndTag(_)) => break XML_EVENT_END_TAG,
                Some(crate::apk::axml::XmlEventKind::Text(_)) => break XML_EVENT_TEXT,
                Some(crate::apk::axml::XmlEventKind::StartNamespace(_))
                | Some(crate::apk::axml::XmlEventKind::EndNamespace(_)) => continue,
                None => break XML_EVENT_END_DOCUMENT,
            }
        });
        match event {
            Ok(ev) => Ok(ev),
            Err(e) => {
                tracing::warn!(
                    target: "android.content.res.XmlBlock",
                    state,
                    error = %e,
                    "XmlBlock.nativeNext: invalid state handle → END_DOCUMENT"
                );
                Ok(XML_EVENT_END_DOCUMENT)
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn xml_block_destroy_parse_state<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    state: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = xml_registry::with_block(state, |_b| ()) {
            tracing::debug!(
                target: "android.content.res.XmlBlock",
                state,
                error = %e,
                "XmlBlock.nativeDestroyParseState: handle invalid (ignored)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn xml_block_get_name<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    state: jlong,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        let name =
            xml_registry::with_block(state, |b| b.current_element().and_then(|e| e.name.clone()))
                .ok()
                .flatten();
        match name {
            Some(n) => env.new_string(n),

            None => Ok(JString::default()),
        }
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn xml_block_destroy<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    block: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = xml_registry::free(block) {
            tracing::debug!(
                target: "android.content.res.XmlBlock",
                block,
                error = %e,
                "XmlBlock.nativeDestroy: handle already freed/invalid (ignored)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn xml_block_get_attribute_index<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    state: jlong,
    namespace: JString<'local>,
    name: JString<'local>,
) -> jint {
    env.with_env(|env| -> jni::errors::Result<jint> {
        if name.is_null() {
            return Ok(XML_ATTR_NOT_FOUND);
        }
        let want_name = name.try_to_string(env)?;

        let want_ns = if namespace.is_null() {
            String::new()
        } else {
            namespace.try_to_string(env)?
        };
        let idx = xml_registry::with_block(state, |b| {
            b.current_element().and_then(|e| {
                e.attributes.iter().position(|a| {
                    let a_ns = a.namespace.as_deref().unwrap_or("");
                    let a_name = a.name.as_deref().unwrap_or("");
                    a_name == want_name && a_ns == want_ns
                })
            })
        })
        .ok()
        .flatten();
        match idx {
            Some(i) => Ok(jint::try_from(i).unwrap_or(XML_ATTR_NOT_FOUND)),
            None => Ok(XML_ATTR_NOT_FOUND),
        }
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn xml_block_get_attribute_string_value<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    state: jlong,
    idx: jint,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        let value = current_attribute(state, idx, |a| a.value_string.clone());
        match value.flatten() {
            Some(s) => env.new_string(s),
            None => Ok(JString::default()),
        }
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn xml_block_get_attribute_data_type<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    state: jlong,
    idx: jint,
) -> jint {
    env.with_env(|_env| -> jni::errors::Result<jint> {
        let data_type = current_attribute(state, idx, |a| jint::from(a.value_type));
        Ok(data_type.unwrap_or(XML_TYPE_NULL))
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn xml_block_get_attribute_data<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    state: jlong,
    idx: jint,
) -> jint {
    env.with_env(|_env| -> jni::errors::Result<jint> {
        let data = current_attribute(state, idx, |a| a.value_data as i32);
        Ok(data.unwrap_or(0))
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn xml_block_get_attribute_count<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    state: jlong,
) -> jint {
    env.with_env(|_env| -> jni::errors::Result<jint> {
        let count = xml_registry::with_block(state, |b| {
            b.current_element().map(|e| e.attributes.len()).unwrap_or(0)
        })
        .unwrap_or(0);
        Ok(jint::try_from(count).unwrap_or(jint::MAX))
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn xml_block_get_attribute_resource<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    state: jlong,
    idx: jint,
) -> jint {
    env.with_env(|_env| -> jni::errors::Result<jint> {
        let res = current_attribute(state, idx, |a| u32_to_i32(a.name_resource));
        Ok(res.unwrap_or(0))
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn xml_block_get_line_number<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    state: jlong,
) -> jint {
    env.with_env(|_env| -> jni::errors::Result<jint> {
        if let Err(e) = xml_registry::with_block(state, |_b| ()) {
            tracing::debug!(
                target: "android.content.res.XmlBlock",
                state,
                error = %e,
                "XmlBlock.nativeGetLineNumber: invalid state handle → -1 (unknown)"
            );
        }

        Ok(XML_LINE_UNKNOWN)
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn xml_block_get_pooled_string<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    state: jlong,
    idx: jint,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        let value = usize::try_from(idx).ok().and_then(|i| {
            xml_registry::with_block(state, |b| b.pooled_string(i).map(str::to_owned))
                .ok()
                .flatten()
        });
        match value {
            Some(s) => env.new_string(s),
            None => Ok(JString::default()),
        }
    })
    .resolve::<LogErrorAndDefault>()
}

fn current_attribute<R>(
    state: jlong,
    idx: jint,
    f: impl FnOnce(&crate::apk::axml::XmlAttribute) -> R,
) -> Option<R> {
    let i = usize::try_from(idx).ok()?;
    xml_registry::with_block(state, |b| {
        b.current_element().and_then(|e| e.attributes.get(i)).map(f)
    })
    .ok()
    .flatten()
}

fn register_xml_block_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(XML_BLOCK_CLASS)?;
    let methods = [
        unsafe {
            NativeMethod::from_raw_parts(
                XML_BLOCK_CREATE_PARSE_STATE_NAME,
                XML_BLOCK_CREATE_PARSE_STATE_SIG,
                xml_block_create_parse_state as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                XML_BLOCK_NEXT_NAME,
                XML_BLOCK_NEXT_SIG,
                xml_block_next as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                XML_BLOCK_DESTROY_PARSE_STATE_NAME,
                XML_BLOCK_DESTROY_PARSE_STATE_SIG,
                xml_block_destroy_parse_state as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                XML_BLOCK_GET_NAME_NAME,
                XML_BLOCK_GET_NAME_SIG,
                xml_block_get_name as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                XML_BLOCK_DESTROY_NAME,
                XML_BLOCK_DESTROY_SIG,
                xml_block_destroy as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                XML_BLOCK_GET_ATTR_INDEX_NAME,
                XML_BLOCK_GET_ATTR_INDEX_SIG,
                xml_block_get_attribute_index as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                XML_BLOCK_GET_ATTR_STRING_VALUE_NAME,
                XML_BLOCK_GET_ATTR_STRING_VALUE_SIG,
                xml_block_get_attribute_string_value as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                XML_BLOCK_GET_ATTR_COUNT_NAME,
                XML_BLOCK_GET_ATTR_COUNT_SIG,
                xml_block_get_attribute_count as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                XML_BLOCK_GET_ATTR_RESOURCE_NAME,
                XML_BLOCK_GET_ATTR_RESOURCE_SIG,
                xml_block_get_attribute_resource as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                XML_BLOCK_GET_ATTR_DATA_TYPE_NAME,
                XML_BLOCK_GET_ATTR_DATA_TYPE_SIG,
                xml_block_get_attribute_data_type as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                XML_BLOCK_GET_ATTR_DATA_NAME,
                XML_BLOCK_GET_ATTR_DATA_SIG,
                xml_block_get_attribute_data as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                XML_BLOCK_GET_LINE_NUMBER_NAME,
                XML_BLOCK_GET_LINE_NUMBER_SIG,
                xml_block_get_line_number as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                XML_BLOCK_GET_POOLED_STRING_NAME,
                XML_BLOCK_GET_POOLED_STRING_SIG,
                xml_block_get_pooled_string as *mut std::ffi::c_void,
            )
        },
    ];

    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/content/res/XmlBlock",
        "registered Eclipse's non-GTK backing for XmlBlock parser natives (nativeCreateParseState/nativeNext/nativeDestroyParseState/nativeGetName/nativeDestroy/nativeGetLineNumber/nativeGetPooledString)"
    );
    Ok(())
}

pub const ENVIRONMENT_CLASS: &JNIStr = jni_str!("android/os/Environment");

const GET_APP_DATA_DIR_NAME: &JNIStr = jni_str!("native_get_app_data_dir");
const GET_APP_DATA_DIR_SIG: &JNIStr = jni_str!("()Ljava/lang/String;");

pub fn app_data_dir() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("ECLIPSE_APP_DATA_DIR") {
        return Some(std::path::PathBuf::from(dir));
    }
    let dirs = directories::ProjectDirs::from("", "", "eclipse")?;
    Some(dirs.data_dir().join("app-data"))
}

extern "system" fn native_get_app_data_dir<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        let dir =
            app_data_dir().ok_or(jni::errors::Error::JniCall(jni::errors::JniError::Unknown))?;

        env.new_string(dir.to_string_lossy())
    })
    .resolve::<LogErrorAndDefault>()
}

fn register_environment_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(ENVIRONMENT_CLASS)?;
    let methods = [unsafe {
        NativeMethod::from_raw_parts(
            GET_APP_DATA_DIR_NAME,
            GET_APP_DATA_DIR_SIG,
            native_get_app_data_dir as *mut std::ffi::c_void,
        )
    }];

    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/os/Environment",
        "registered Eclipse's non-GTK backing for native_get_app_data_dir"
    );
    Ok(())
}

pub const SYSTEM_CLOCK_CLASS: &JNIStr = jni_str!("android/os/SystemClock");

const ELAPSED_REALTIME_NAME: &JNIStr = jni_str!("elapsedRealtime");
const ELAPSED_REALTIME_SIG: &JNIStr = jni_str!("()J");
const ELAPSED_REALTIME_NANOS_NAME: &JNIStr = jni_str!("elapsedRealtimeNanos");
const ELAPSED_REALTIME_NANOS_SIG: &JNIStr = jni_str!("()J");

const UPTIME_MILLIS_NAME: &JNIStr = jni_str!("uptimeMillis");
const UPTIME_MILLIS_SIG: &JNIStr = jni_str!("()J");

const CURRENT_THREAD_TIME_MILLIS_NAME: &JNIStr = jni_str!("currentThreadTimeMillis");
const CURRENT_THREAD_TIME_MILLIS_SIG: &JNIStr = jni_str!("()J");

static MONOTONIC_ANCHOR: OnceLock<Instant> = OnceLock::new();

const ATL_PROCESS_START_MILLIS: u64 = 1000;

extern "system" fn system_clock_elapsed_realtime<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> { Ok(monotonic_millis()) })
        .resolve::<LogErrorAndDefault>()
}

extern "system" fn system_clock_uptime_millis<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> { Ok(monotonic_millis()) })
        .resolve::<LogErrorAndDefault>()
}

extern "system" fn system_clock_elapsed_realtime_nanos<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> { Ok(monotonic_nanos()) })
        .resolve::<LogErrorAndDefault>()
}

extern "system" fn system_clock_current_thread_time_millis<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        Ok(cpu_clock_millis(libc::CLOCK_THREAD_CPUTIME_ID))
    })
    .resolve::<LogErrorAndDefault>()
}

fn since_process_start() -> std::time::Duration {
    std::time::Duration::from_millis(ATL_PROCESS_START_MILLIS)
        + MONOTONIC_ANCHOR.get_or_init(Instant::now).elapsed()
}

fn monotonic_millis() -> jlong {
    jlong::try_from(since_process_start().as_millis()).unwrap_or(jlong::MAX)
}

fn monotonic_nanos() -> jlong {
    jlong::try_from(since_process_start().as_nanos()).unwrap_or(jlong::MAX)
}

fn register_system_clock_natives(env: &mut Env) -> Result<(), FrameworkError> {
    MONOTONIC_ANCHOR.get_or_init(Instant::now);
    let class = env.find_class(SYSTEM_CLOCK_CLASS)?;
    let methods = [
        unsafe {
            NativeMethod::from_raw_parts(
                ELAPSED_REALTIME_NAME,
                ELAPSED_REALTIME_SIG,
                system_clock_elapsed_realtime as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                ELAPSED_REALTIME_NANOS_NAME,
                ELAPSED_REALTIME_NANOS_SIG,
                system_clock_elapsed_realtime_nanos as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                UPTIME_MILLIS_NAME,
                UPTIME_MILLIS_SIG,
                system_clock_uptime_millis as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                CURRENT_THREAD_TIME_MILLIS_NAME,
                CURRENT_THREAD_TIME_MILLIS_SIG,
                system_clock_current_thread_time_millis as *mut std::ffi::c_void,
            )
        },
    ];

    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/os/SystemClock",
        "registered Eclipse's non-GTK backing for elapsedRealtime, elapsedRealtimeNanos, uptimeMillis, and currentThreadTimeMillis"
    );
    Ok(())
}

pub const MESSAGE_QUEUE_CLASS: &JNIStr = jni_str!("android/os/MessageQueue");

const MESSAGE_QUEUE_NATIVE_INIT_NAME: &JNIStr = jni_str!("nativeInit");
const MESSAGE_QUEUE_NATIVE_INIT_SIG: &JNIStr = jni_str!("()J");
const MESSAGE_QUEUE_NATIVE_DESTROY_NAME: &JNIStr = jni_str!("nativeDestroy");
const MESSAGE_QUEUE_NATIVE_DESTROY_SIG: &JNIStr = jni_str!("(J)V");
const MESSAGE_QUEUE_NATIVE_IS_IDLING_NAME: &JNIStr = jni_str!("nativeIsIdling");
const MESSAGE_QUEUE_NATIVE_IS_IDLING_SIG: &JNIStr = jni_str!("(J)Z");
const MESSAGE_QUEUE_NATIVE_POLL_ONCE_NAME: &JNIStr = jni_str!("nativePollOnce");
const MESSAGE_QUEUE_NATIVE_POLL_ONCE_SIG: &JNIStr = jni_str!("(JI)Z");
const MESSAGE_QUEUE_NATIVE_WAKE_NAME: &JNIStr = jni_str!("nativeWake");
const MESSAGE_QUEUE_NATIVE_WAKE_SIG: &JNIStr = jni_str!("(J)V");

static ANDROID_MAIN_THREAD_ID: std::sync::OnceLock<std::thread::ThreadId> =
    std::sync::OnceLock::new();

thread_local! {

    static MAIN_LOOPER_PUMP_IN_PROGRESS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

static MAIN_LOOPER_PUMP_ACTIVE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

extern "system" fn message_queue_native_init<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        let current_thread = std::thread::current().id();
        let role = if ANDROID_MAIN_THREAD_ID
            .get()
            .is_none_or(|main_thread| *main_thread == current_thread)
        {
            message_queue::QueueRole::Main
        } else {
            message_queue::QueueRole::Worker
        };
        let Some(handle) = message_queue::create(role) else {
            tracing::error!(
                target: "android.os.MessageQueue",
                "MessageQueue.nativeInit: host handle allocation failed"
            );
            return Ok(0);
        };
        tracing::debug!(
            target: "android.os.MessageQueue",
            handle,
            ?role,
            "MessageQueue.nativeInit: allocated host queue handle"
        );
        Ok(handle)
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn message_queue_native_destroy<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    ptr: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if !message_queue::destroy(ptr) {
            tracing::debug!(
                target: "android.os.MessageQueue",
                handle = ptr,
                "MessageQueue.nativeDestroy: ignored stale queue handle"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn message_queue_native_is_idling<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    ptr: jlong,
) -> jboolean {
    env.with_env(|_env| -> jni::errors::Result<jboolean> { Ok(message_queue::is_idling(ptr)) })
        .resolve::<LogErrorAndDefault>()
}

extern "system" fn message_queue_native_poll_once<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    ptr: jlong,
    timeout_millis: jint,
) -> jboolean {
    env.with_env(|_env| -> jni::errors::Result<jboolean> {
        Ok(message_queue::poll_should_yield(ptr, timeout_millis))
    })
    .resolve::<LogErrorAndDefault>()
}

#[cfg(test)]
fn main_looper_poll_should_yield(timeout_millis: jint) -> bool {
    timeout_millis != 0
}

extern "system" fn message_queue_native_wake<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    ptr: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        match message_queue::wake(ptr) {
            Some(message_queue::QueueRole::Main) => wake_main_looper(),
            Some(message_queue::QueueRole::Worker) => {}
            None => tracing::debug!(
                target: "android.os.MessageQueue",
                handle = ptr,
                "MessageQueue.nativeWake: ignored stale queue handle"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

fn register_message_queue_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let registering_thread = std::thread::current().id();
    let main_thread = ANDROID_MAIN_THREAD_ID.get_or_init(|| registering_thread);
    if *main_thread != registering_thread {
        tracing::warn!(
            target: "android.os.MessageQueue",
            expected = ?main_thread,
            actual = ?registering_thread,
            "MessageQueue natives registered from a different thread than the established Android main thread"
        );
    }

    let class = env.find_class(MESSAGE_QUEUE_CLASS)?;
    let methods = [
        unsafe {
            NativeMethod::from_raw_parts(
                MESSAGE_QUEUE_NATIVE_INIT_NAME,
                MESSAGE_QUEUE_NATIVE_INIT_SIG,
                message_queue_native_init as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                MESSAGE_QUEUE_NATIVE_DESTROY_NAME,
                MESSAGE_QUEUE_NATIVE_DESTROY_SIG,
                message_queue_native_destroy as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                MESSAGE_QUEUE_NATIVE_IS_IDLING_NAME,
                MESSAGE_QUEUE_NATIVE_IS_IDLING_SIG,
                message_queue_native_is_idling as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                MESSAGE_QUEUE_NATIVE_POLL_ONCE_NAME,
                MESSAGE_QUEUE_NATIVE_POLL_ONCE_SIG,
                message_queue_native_poll_once as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                MESSAGE_QUEUE_NATIVE_WAKE_NAME,
                MESSAGE_QUEUE_NATIVE_WAKE_SIG,
                message_queue_native_wake as *mut std::ffi::c_void,
            )
        },
    ];

    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/os/MessageQueue",
        "registered Eclipse's host queue lifecycle + main/worker poll backing"
    );
    Ok(())
}

pub const SENSOR_MANAGER_CLASS: &JNIStr = jni_str!("android/hardware/SensorManager");

const SENSOR_MANAGER_REGISTER_NAME: &JNIStr = jni_str!("register_accelerometer_listener_native");
const SENSOR_MANAGER_REGISTER_SIG: &JNIStr =
    jni_str!("(Landroid/hardware/SensorEventListener;Landroid/hardware/Sensor;I)V");

extern "system" fn sensor_manager_register_accelerometer_listener<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    _listener: JObject<'local>,
    _sensor: JObject<'local>,
    rate: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        tracing::debug!(
            target: "android.hardware.SensorManager",
            rate,
            "SensorManager.register_accelerometer_listener_native: no accelerometer on this host; \
             registering no source, delivering no events (honest no-sensor)"
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

fn register_sensor_manager_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(SENSOR_MANAGER_CLASS)?;
    let methods = [unsafe {
        NativeMethod::from_raw_parts(
            SENSOR_MANAGER_REGISTER_NAME,
            SENSOR_MANAGER_REGISTER_SIG,
            sensor_manager_register_accelerometer_listener as *mut std::ffi::c_void,
        )
    }];

    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/hardware/SensorManager",
        "registered Eclipse's honest no-sensor backing for register_accelerometer_listener_native"
    );
    Ok(())
}

pub const VIBRATOR_CLASS: &JNIStr = jni_str!("android/os/Vibrator");

const VIBRATOR_NATIVE_CONSTRUCTOR_NAME: &JNIStr = jni_str!("native_constructor");
const VIBRATOR_NATIVE_CONSTRUCTOR_SIG: &JNIStr = jni_str!("()I");

const VIBRATOR_NATIVE_VIBRATE_NAME: &JNIStr = jni_str!("native_vibrate");
const VIBRATOR_NATIVE_VIBRATE_SIG: &JNIStr = jni_str!("(IJ)V");

extern "system" fn vibrator_native_constructor<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
) -> jint {
    env.with_env(|_env| -> jni::errors::Result<jint> {
        tracing::debug!(
            target: "android.os.Vibrator",
            "Vibrator.native_constructor: no vibration device on the host (fd = -1)"
        );
        Ok(-1)
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn vibrator_native_vibrate<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    fd: jint,
    millis: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        tracing::debug!(
            target: "android.os.Vibrator",
            fd,
            millis,
            "Vibrator.native_vibrate: no-op (no vibration motor on the host)"
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

fn register_vibrator_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(VIBRATOR_CLASS)?;
    let methods = [
        unsafe {
            NativeMethod::from_raw_parts(
                VIBRATOR_NATIVE_CONSTRUCTOR_NAME,
                VIBRATOR_NATIVE_CONSTRUCTOR_SIG,
                vibrator_native_constructor as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                VIBRATOR_NATIVE_VIBRATE_NAME,
                VIBRATOR_NATIVE_VIBRATE_SIG,
                vibrator_native_vibrate as *mut std::ffi::c_void,
            )
        },
    ];

    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/os/Vibrator",
        "registered Eclipse's no-vibration-device backing for native_constructor + native_vibrate"
    );
    Ok(())
}

pub const PROCESS_CLASS: &JNIStr = jni_str!("android/os/Process");

const PROCESS_GET_ELAPSED_CPU_TIME_NAME: &JNIStr = jni_str!("getElapsedCpuTime");
const PROCESS_GET_ELAPSED_CPU_TIME_SIG: &JNIStr = jni_str!("()J");
const PROCESS_IS_64_BIT_NAME: &JNIStr = jni_str!("is64Bit");
const PROCESS_IS_64_BIT_SIG: &JNIStr = jni_str!("()Z");
const PROCESS_GET_THREAD_PRIORITY_NAME: &JNIStr = jni_str!("getThreadPriority");
const PROCESS_GET_THREAD_PRIORITY_SIG: &JNIStr = jni_str!("(I)I");

fn cpu_clock_millis(clock: libc::clockid_t) -> jlong {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };

    let rc = unsafe { libc::clock_gettime(clock, &mut ts) };
    if rc != 0 {
        return 0;
    }

    ts.tv_sec
        .saturating_mul(1000)
        .saturating_add(ts.tv_nsec / 1_000_000)
}

extern "system" fn process_get_elapsed_cpu_time<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        Ok(cpu_clock_millis(libc::CLOCK_PROCESS_CPUTIME_ID))
    })
    .resolve::<LogErrorAndDefault>()
}

fn thread_priority(tid: jint) -> std::io::Result<jint> {
    let who = libc::id_t::from_ne_bytes(tid.to_ne_bytes());
    unsafe { *libc::__errno_location() = 0 };
    let priority = unsafe { libc::getpriority(libc::PRIO_PROCESS, who) };
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(0) => Ok(priority),
        _ => Err(error),
    }
}

extern "system" fn process_get_thread_priority<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    tid: jint,
) -> jint {
    env.with_env(|env| -> jni::errors::Result<jint> {
        let error = match thread_priority(tid) {
            Ok(priority) => return Ok(priority),
            Err(error) => error,
        };
        let (class, message) = match error.raw_os_error() {
            Some(libc::ESRCH) => (
                jni_str!("java/lang/IllegalArgumentException"),
                format!("Given thread {tid} does not exist"),
            ),
            Some(libc::EPERM | libc::EACCES) => (
                jni_str!("java/lang/SecurityException"),
                format!("No permission to read the priority of {tid}"),
            ),
            _ => (
                jni_str!("java/lang/RuntimeException"),
                format!("getpriority({tid}) failed: {error}"),
            ),
        };
        let _ = env.throw_new(class, JNIString::from(message));
        Ok(0)
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn process_is_64_bit<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jboolean {
    env.with_env(|_env| -> jni::errors::Result<jboolean> { Ok(cfg!(target_pointer_width = "64")) })
        .resolve::<LogErrorAndDefault>()
}

fn register_process_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(PROCESS_CLASS)?;
    let methods = [
        unsafe {
            NativeMethod::from_raw_parts(
                PROCESS_GET_ELAPSED_CPU_TIME_NAME,
                PROCESS_GET_ELAPSED_CPU_TIME_SIG,
                process_get_elapsed_cpu_time as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                PROCESS_IS_64_BIT_NAME,
                PROCESS_IS_64_BIT_SIG,
                process_is_64_bit as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                PROCESS_GET_THREAD_PRIORITY_NAME,
                PROCESS_GET_THREAD_PRIORITY_SIG,
                process_get_thread_priority as *mut std::ffi::c_void,
            )
        },
    ];

    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/os/Process",
        "registered Eclipse's backing for getElapsedCpuTime (CLOCK_PROCESS_CPUTIME_ID → ms), is64Bit and getThreadPriority"
    );
    Ok(())
}

pub const INPUT_METHOD_MANAGER_CLASS: &JNIStr =
    jni_str!("android/view/inputmethod/InputMethodManager");

const IMM_NATIVE_INIT_NAME: &JNIStr = jni_str!("nativeInit");
const IMM_NATIVE_INIT_SIG: &JNIStr = jni_str!("()J");
const IMM_NATIVE_HIDE_SOFT_INPUT_NAME: &JNIStr = jni_str!("nativeHideSoftInput");
const IMM_NATIVE_HIDE_SOFT_INPUT_SIG: &JNIStr = jni_str!("(J)V");
const IMM_NATIVE_SHOW_SOFT_INPUT_NAME: &JNIStr = jni_str!("nativeShowSoftInput");
const IMM_NATIVE_SHOW_SOFT_INPUT_SIG: &JNIStr =
    jni_str!("(JJLandroid/view/inputmethod/InputConnection;I)Z");

extern "system" fn imm_native_init<'local>(mut env: EnvUnowned<'local>) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> { Ok(0) })
        .resolve::<LogErrorAndDefault>()
}

extern "system" fn imm_native_hide_soft_input<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    _im_context: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> { Ok(()) })
        .resolve::<LogErrorAndDefault>()
}

extern "system" fn imm_native_show_soft_input<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    _im_context: jlong,
    _view: jlong,
    _ic: JObject<'local>,
    _flags: jint,
) -> jboolean {
    env.with_env(|_env| -> jni::errors::Result<jboolean> { Ok(false) })
        .resolve::<LogErrorAndDefault>()
}

fn register_input_method_manager_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(INPUT_METHOD_MANAGER_CLASS)?;
    let methods = [
        unsafe {
            NativeMethod::from_raw_parts(
                IMM_NATIVE_INIT_NAME,
                IMM_NATIVE_INIT_SIG,
                imm_native_init as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                IMM_NATIVE_HIDE_SOFT_INPUT_NAME,
                IMM_NATIVE_HIDE_SOFT_INPUT_SIG,
                imm_native_hide_soft_input as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                IMM_NATIVE_SHOW_SOFT_INPUT_NAME,
                IMM_NATIVE_SHOW_SOFT_INPUT_SIG,
                imm_native_show_soft_input as *mut std::ffi::c_void,
            )
        },
    ];

    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/view/inputmethod/InputMethodManager",
        "registered Eclipse's non-GTK backing for nativeInit + nativeHideSoftInput + nativeShowSoftInput (no soft keyboard)"
    );
    Ok(())
}

pub const VIEW_CLASS: &JNIStr = jni_str!("android/view/View");

pub const MOTION_EVENT_CLASS: &JNIStr = jni_str!("android/view/MotionEvent");

pub const KEY_EVENT_CLASS: &JNIStr = jni_str!("android/view/KeyEvent");

const VIEW_NATIVE_CONSTRUCTOR_NAME: &JNIStr = jni_str!("native_constructor");
const VIEW_NATIVE_CONSTRUCTOR_SIG: &JNIStr =
    jni_str!("(Landroid/content/Context;Landroid/util/AttributeSet;)J");

const VIEW_NATIVE_SET_PADDING_NAME: &JNIStr = jni_str!("native_setPadding");
const VIEW_NATIVE_SET_PADDING_SIG: &JNIStr = jni_str!("(JIIII)V");

const VIEW_NATIVE_SET_LAYOUT_PARAMS_NAME: &JNIStr = jni_str!("native_setLayoutParams");
const VIEW_NATIVE_SET_LAYOUT_PARAMS_SIG: &JNIStr = jni_str!("(JIIIFIIII)V");

const VIEW_NATIVE_REQUEST_LAYOUT_NAME: &JNIStr = jni_str!("native_requestLayout");
const VIEW_NATIVE_REQUEST_LAYOUT_SIG: &JNIStr = jni_str!("(J)V");

const VIEW_NATIVE_SET_BACKGROUND_DRAWABLE_NAME: &JNIStr = jni_str!("native_setBackgroundDrawable");
const VIEW_NATIVE_SET_BACKGROUND_DRAWABLE_SIG: &JNIStr = jni_str!("(JJ)V");

const VIEW_NATIVE_SET_VISIBILITY_NAME: &JNIStr = jni_str!("native_setVisibility");
const VIEW_NATIVE_SET_VISIBILITY_SIG: &JNIStr = jni_str!("(JIFZ)V");

const VIEW_SET_ON_CLICK_LISTENER_NAME: &JNIStr = jni_str!("nativeSetOnClickListener");
const VIEW_SET_ON_CLICK_LISTENER_SIG: &JNIStr = jni_str!("(J)V");

const VIEW_SET_ON_TOUCH_LISTENER_NAME: &JNIStr = jni_str!("nativeSetOnTouchListener");
const VIEW_SET_ON_TOUCH_LISTENER_SIG: &JNIStr = jni_str!("(J)V");
const VIEW_SET_ON_LONG_CLICK_LISTENER_NAME: &JNIStr = jni_str!("nativeSetOnLongClickListener");
const VIEW_SET_ON_LONG_CLICK_LISTENER_SIG: &JNIStr = jni_str!("(J)V");

const VIEW_SET_BACKGROUND_COLOR_NAME: &JNIStr = jni_str!("native_setBackgroundColor");
const VIEW_SET_BACKGROUND_COLOR_SIG: &JNIStr = jni_str!("(JI)V");

const VIEW_NATIVE_SET_FULLSCREEN_NAME: &JNIStr = jni_str!("nativeSetFullscreen");
const VIEW_NATIVE_SET_FULLSCREEN_SIG: &JNIStr = jni_str!("(JZ)V");

const VIEW_NATIVE_GET_WINDOW_NAME: &JNIStr = jni_str!("native_get_window");
const VIEW_NATIVE_GET_WINDOW_SIG: &JNIStr = jni_str!("(J)Landroid/view/Window;");

const VIEW_NATIVE_DESTRUCTOR_NAME: &JNIStr = jni_str!("native_destructor");
const VIEW_NATIVE_DESTRUCTOR_SIG: &JNIStr = jni_str!("(J)V");

const VIEW_GET_WINDOW_VISIBLE_DISPLAY_FRAME_NAME: &JNIStr =
    jni_str!("getWindowVisibleDisplayFrame");
const VIEW_GET_WINDOW_VISIBLE_DISPLAY_FRAME_SIG: &JNIStr = jni_str!("(Landroid/graphics/Rect;)V");
const VIEW_NATIVE_IS_ATTACHED_TO_WINDOW_NAME: &JNIStr = jni_str!("nativeIsAttachedToWindow");
const VIEW_NATIVE_IS_ATTACHED_TO_WINDOW_SIG: &JNIStr = jni_str!("(J)Z");

const VIEW_NATIVE_GET_GLOBAL_VISIBLE_RECT_NAME: &JNIStr = jni_str!("native_getGlobalVisibleRect");
const VIEW_NATIVE_GET_GLOBAL_VISIBLE_RECT_SIG: &JNIStr = jni_str!("(JLandroid/graphics/Rect;)Z");
const VIEW_NATIVE_REQUEST_FOCUS_NAME: &JNIStr = jni_str!("nativeRequestFocus");
const VIEW_NATIVE_REQUEST_FOCUS_SIG: &JNIStr = jni_str!("(JI)V");

const VIEW_NATIVE_LAYOUT_NAME: &JNIStr = jni_str!("native_layout");
const VIEW_NATIVE_LAYOUT_SIG: &JNIStr = jni_str!("(JIIII)V");

const VIEW_NATIVE_INVALIDATE_NAME: &JNIStr = jni_str!("nativeInvalidate");
const VIEW_NATIVE_INVALIDATE_SIG: &JNIStr = jni_str!("(J)V");

const VIEW_NATIVE_IS_FOCUSED_NAME: &JNIStr = jni_str!("nativeIsFocused");
const VIEW_NATIVE_IS_FOCUSED_SIG: &JNIStr = jni_str!("(J)Z");

const VIEW_NATIVE_KEEP_SCREEN_ON_NAME: &JNIStr = jni_str!("native_keep_screen_on");
const VIEW_NATIVE_KEEP_SCREEN_ON_SIG: &JNIStr = jni_str!("(JZ)V");

const VIEW_NATIVE_ADD_CLASS_NAME: &JNIStr = jni_str!("native_addClass");
const VIEW_NATIVE_ADD_CLASS_SIG: &JNIStr = jni_str!("(JLjava/lang/String;)V");
const VIEW_NATIVE_REMOVE_CLASSES_NAME: &JNIStr = jni_str!("native_removeClasses");
const VIEW_NATIVE_REMOVE_CLASSES_SIG: &JNIStr = jni_str!("(J[Ljava/lang/String;)V");

const VIEW_NATIVE_DRAW_BACKGROUND_NAME: &JNIStr = jni_str!("native_drawBackground");
const VIEW_NATIVE_DRAW_BACKGROUND_SIG: &JNIStr = jni_str!("(JJ)V");
const VIEW_NATIVE_DRAW_CONTENT_NAME: &JNIStr = jni_str!("native_drawContent");
const VIEW_NATIVE_DRAW_CONTENT_SIG: &JNIStr = jni_str!("(JJ)V");

const VIEW_NATIVE_QUEUE_ALLOCATE_NAME: &JNIStr = jni_str!("native_queueAllocate");
const VIEW_NATIVE_QUEUE_ALLOCATE_SIG: &JNIStr = jni_str!("(J)V");

const VIEW_NATIVE_MEASURE_NAME: &JNIStr = jni_str!("native_measure");
const VIEW_NATIVE_MEASURE_SIG: &JNIStr = jni_str!("(JII)V");

const VIEW_SET_MEASURED_DIMENSION_NAME: &JNIStr = jni_str!("setMeasuredDimension");
const VIEW_SET_MEASURED_DIMENSION_SIG: MethodSignature<'static, 'static> = jni_sig!("(II)V");
const VIEW_GET_SUGGESTED_MIN_WIDTH_NAME: &JNIStr = jni_str!("getSuggestedMinimumWidth");
const VIEW_GET_SUGGESTED_MIN_HEIGHT_NAME: &JNIStr = jni_str!("getSuggestedMinimumHeight");
const VIEW_GET_SUGGESTED_MIN_SIG: MethodSignature<'static, 'static> = jni_sig!("()I");

const MEASURE_SPEC_MODE_MASK: jint = (0x3u32 << 30) as jint;
const MEASURE_SPEC_EXACTLY: jint = (1u32 << 30) as jint;
const MEASURE_SPEC_AT_MOST: jint = (2u32 << 30) as jint;

fn measure_default_size(measure_spec: jint, suggested_minimum: jint) -> jint {
    match measure_spec & MEASURE_SPEC_MODE_MASK {
        m if m == MEASURE_SPEC_EXACTLY || m == MEASURE_SPEC_AT_MOST => {
            measure_spec & !MEASURE_SPEC_MODE_MASK
        }
        _ => suggested_minimum,
    }
}

extern "system" fn view_native_constructor<'local>(
    mut env: EnvUnowned<'local>,
    this: JObject<'local>,
    _context: JObject<'local>,
    _attrs: JObject<'local>,
) -> jlong {
    env.with_env(|env| -> jni::errors::Result<jlong> {
        let class_name = view_class_name(env, &this).unwrap_or_default();
        match view_registry::allocate(&class_name) {
            Ok(handle) => {

                match env.new_weak_ref(&this) {
                    Ok(peer) => {
                        if let Err(e) =
                            view_registry::set_jobject(handle, view_registry::ViewObject::Peer(peer))
                        {
                            tracing::debug!(
                                target: "android.view.View",
                                class = %class_name,
                                handle,
                                error = %e,
                                "View.native_constructor: could not store view jobject (non-dispatchable)"
                            );
                        }
                    }
                    Err(e) => tracing::debug!(
                        target: "android.view.View",
                        class = %class_name,
                        handle,
                        error = %e,
                        "View.native_constructor: new_weak_ref failed (view non-dispatchable)"
                    ),
                }
                tracing::debug!(
                    target: "android.view.View",
                    class = %class_name,
                    handle,
                    "View.native_constructor: allocated non-GTK view-registry peer"
                );
                Ok(handle)
            }
            Err(e) => {
                tracing::warn!(
                    target: "android.view.View",
                    class = %class_name,
                    error = %e,
                    "View.native_constructor: view-registry allocate failed → 0 (no peer)"
                );
                Ok(0)
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

fn view_class_name(env: &mut Env, obj: &JObject) -> Option<String> {
    let class = env.get_object_class(obj).ok()?;
    let name = env
        .call_method(
            &class,
            jni_str!("getName"),
            jni_sig!("()Ljava/lang/String;"),
            &[],
        )
        .ok()?
        .l()
        .ok()?;

    let name = JString::cast_local(env, name).ok()?;
    name.try_to_string(env).ok()
}

extern "system" fn view_native_set_padding<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    left: jint,
    top: jint,
    right: jint,
    bottom: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        match view_registry::with_view(widget, |v| {
            v.layout.padding = [left, top, right, bottom];
        }) {
            Ok(()) => tracing::trace!(
                target: "android.view.View",
                widget, left, top, right, bottom,
                "View.native_setPadding: recorded padding on view peer"
            ),
            Err(e) => tracing::debug!(
                target: "android.view.View",
                widget,
                error = %e,
                "View.native_setPadding: invalid view handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn view_native_set_layout_params<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    width: jint,
    height: jint,
    gravity: jint,
    weight: f32,
    left_margin: jint,
    top_margin: jint,
    right_margin: jint,
    bottom_margin: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        match view_registry::with_view(widget, |v| {
            v.layout.width = width;
            v.layout.height = height;
            v.layout.gravity = gravity;
            v.layout.weight = weight;
            v.layout.margins = [left_margin, top_margin, right_margin, bottom_margin];
        }) {
            Ok(()) => {
                tracing::trace!(
                    target: "android.view.View",
                    widget, width, height, gravity, weight,
                    "View.native_setLayoutParams: recorded layout params on view peer"
                );
                request_textbox_refresh(widget);
            }
            Err(e) => tracing::debug!(
                target: "android.view.View",
                widget,
                error = %e,
                "View.native_setLayoutParams: invalid view handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn view_native_request_layout<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = view_registry::with_view(widget, |_v| ()) {
            tracing::debug!(
                target: "android.view.View",
                widget,
                error = %e,
                "View.native_requestLayout: invalid view handle (ignored)"
            );
        } else {
            mark_global_layout_pending();
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn view_native_set_background_drawable<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    drawable: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = view_registry::with_view(widget, |_v| ()) {
            tracing::debug!(
                target: "android.view.View",
                widget,
                drawable,
                error = %e,
                "View.native_setBackgroundDrawable: invalid view handle (ignored)"
            );
        } else {
            tracing::trace!(
                target: "android.view.View",
                widget,
                drawable,
                "View.native_setBackgroundDrawable: validated handle, no-op (drawable draw deferred)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn view_native_set_visibility<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    visibility: jint,
    alpha: f32,
    sensitive: jboolean,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if let Err(e) = view_registry::set_visible(widget, visibility == VIEW_VISIBLE) {
            tracing::debug!(
                target: "android.view.View",
                widget,
                visibility,
                alpha,
                sensitive,
                error = %e,
                "View.native_setVisibility: invalid view handle (ignored)"
            );
        } else {
            tracing::trace!(
                target: "android.view.View",
                widget,
                visibility,
                alpha,
                sensitive,
                "View.native_setVisibility: recorded the view's visibility"
            );
            text_field_visibility_changed(widget, visibility);
            crate::webview::client::refresh_visibility(env);
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn view_native_set_background_color<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    color: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        match view_registry::set_background_color(widget, color) {
            Ok(()) => tracing::trace!(
                target: "android.view.View",
                widget,
                color = format_args!("0x{:08x}", u32::from_ne_bytes(color.to_ne_bytes())),
                "View.native_setBackgroundColor: recorded background color on view peer"
            ),
            Err(e) => tracing::debug!(
                target: "android.view.View",
                widget,
                error = %e,
                "View.native_setBackgroundColor: invalid view handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn view_native_set_fullscreen<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    fullscreen: jboolean,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = view_registry::with_view(widget, |_v| ()) {
            tracing::debug!(
                target: "android.view.View",
                widget,
                fullscreen,
                error = %e,
                "View.nativeSetFullscreen: invalid view handle (ignored)"
            );
        } else {
            tracing::trace!(
                target: "android.view.View",
                widget,
                fullscreen,
                "View.nativeSetFullscreen: validated handle, no-op (no system bars on the host window)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn view_get_window_visible_display_frame<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    rect: JObject<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if rect.is_null() {
            return Ok(());
        }
        let (w, h) = crate::loader::ndk_registry::engine_window_geometry().unwrap_or((800, 600));

        let int_sig =
            unsafe { FieldSignature::from_raw_parts(INT_SIG, JavaType::Primitive(Primitive::Int)) };
        env.set_field(&rect, jni_str!("left"), &int_sig, 0i32.into())?;
        env.set_field(&rect, jni_str!("top"), &int_sig, 0i32.into())?;
        env.set_field(&rect, jni_str!("right"), &int_sig, w.into())?;
        env.set_field(&rect, jni_str!("bottom"), &int_sig, h.into())?;
        tracing::trace!(
            target: "android.view.View",
            w,
            h,
            "View.getWindowVisibleDisplayFrame: filled with the host window frame"
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn view_native_is_attached_to_window<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    _widget: jlong,
) -> jboolean {
    env.with_env(|_env| -> jni::errors::Result<jboolean> { Ok(true) })
        .resolve::<LogErrorAndDefault>()
}

extern "system" fn view_native_get_global_visible_rect<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    _widget: jlong,
    rect: JObject<'local>,
) -> jboolean {
    env.with_env(|env| -> jni::errors::Result<jboolean> {
        if rect.is_null() {
            return Ok(false);
        }
        let (w, h) = crate::loader::ndk_registry::engine_window_geometry().unwrap_or((800, 600));

        let int_sig =
            unsafe { FieldSignature::from_raw_parts(INT_SIG, JavaType::Primitive(Primitive::Int)) };
        env.set_field(&rect, jni_str!("left"), &int_sig, 0i32.into())?;
        env.set_field(&rect, jni_str!("top"), &int_sig, 0i32.into())?;
        env.set_field(&rect, jni_str!("right"), &int_sig, w.into())?;
        env.set_field(&rect, jni_str!("bottom"), &int_sig, h.into())?;
        tracing::trace!(
            target: "android.view.View",
            w,
            h,
            "View.native_getGlobalVisibleRect: filled with the host window frame"
        );
        Ok(true)
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn view_native_request_focus<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    direction: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        match view_registry::with_view(widget, |_v| ()) {
            Ok(()) => {
                view_registry::set_focused_view(widget);
                tracing::trace!(
                    target: "android.view.View",
                    widget,
                    direction,
                    "View.nativeRequestFocus: recorded focused view"
                );
            }
            Err(e) => tracing::debug!(
                target: "android.view.View",
                widget,
                direction,
                error = %e,
                "View.nativeRequestFocus: invalid view handle (focus record unchanged)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn view_native_is_focused<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    widget: jlong,
) -> jboolean {
    env.with_env(|_env| -> jni::errors::Result<jboolean> {
        let focused = view_registry::is_focused(widget);
        if !focused && widget != 0 && widget == active_text_field() {
            static FOCUS_DIVERGENCE_WARNED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !FOCUS_DIVERGENCE_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::warn!(
                    target: "android.view.View",
                    widget,
                    "View.nativeIsFocused: serving false for the ACTIVE_TEXT_FIELD (the engine-tap \
                     focus signal) — requestFocus never recorded it; if a focus-gated flow \
                     misbehaves this boot, unify the two focus records (see view_native_is_focused)"
                );
            }
        }
        Ok(focused)
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn view_native_layout<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    l: jint,
    t: jint,
    r: jint,
    b: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        match view_registry::set_frame(widget, [l, t, r, b]) {
            Ok(()) => {
                mark_global_layout_pending();
                tracing::trace!(
                    target: "android.view.View",
                    widget,
                    l,
                    t,
                    r,
                    b,
                    "View.native_layout: recorded laid-out frame on view peer"
                );
            }
            Err(e) => tracing::debug!(
                target: "android.view.View",
                widget,
                l,
                t,
                r,
                b,
                error = %e,
                "View.native_layout: invalid view handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn view_native_invalidate<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    widget: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = view_registry::with_view(widget, |_v| ()) {
            tracing::debug!(
                target: "android.view.View",
                widget,
                error = %e,
                "View.nativeInvalidate: invalid view handle (ignored)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn view_native_keep_screen_on<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    widget: jlong,
    keep_screen_on: jboolean,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = view_registry::with_view(widget, |_v| ()) {
            tracing::debug!(
                target: "android.view.View",
                widget,
                keep_screen_on,
                error = %e,
                "View.native_keep_screen_on: invalid view handle (ignored)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn view_native_add_class<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    _class_name: JString<'local>,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = view_registry::with_view(widget, |_v| ()) {
            tracing::debug!(
                target: "android.view.View",
                widget,
                error = %e,
                "View.native_addClass: invalid view handle (ignored)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn view_native_remove_classes<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    _class_names: JObjectArray<'local>,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = view_registry::with_view(widget, |_v| ()) {
            tracing::debug!(
                target: "android.view.View",
                widget,
                error = %e,
                "View.native_removeClasses: invalid view handle (ignored)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn view_native_draw_background<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    snapshot: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = view_registry::with_view(widget, |_v| ()) {
            tracing::debug!(
                target: "android.view.View",
                widget,
                snapshot,
                error = %e,
                "View.native_drawBackground: invalid view handle (ignored)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn view_native_draw_content<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    snapshot: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = view_registry::with_view(widget, |_v| ()) {
            tracing::debug!(
                target: "android.view.View",
                widget,
                snapshot,
                error = %e,
                "View.native_drawContent: invalid view handle (ignored)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn view_native_queue_allocate<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = view_registry::with_view(widget, |_v| ()) {
            tracing::debug!(
                target: "android.view.View",
                widget,
                error = %e,
                "View.native_queueAllocate: invalid view handle (ignored)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn view_native_measure<'local>(
    mut env: EnvUnowned<'local>,
    this: JObject<'local>,
    widget: jlong,
    width_measure_spec: jint,
    height_measure_spec: jint,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if let Err(e) = view_registry::with_view(widget, |_v| ()) {
            tracing::debug!(
                target: "android.view.View",
                widget,
                error = %e,
                "View.native_measure: invalid view handle (measured answer still served)"
            );
        }

        let w_min = suggested_minimum_if_needed(
            env,
            &this,
            width_measure_spec,
            VIEW_GET_SUGGESTED_MIN_WIDTH_NAME,
        );
        let h_min = suggested_minimum_if_needed(
            env,
            &this,
            height_measure_spec,
            VIEW_GET_SUGGESTED_MIN_HEIGHT_NAME,
        );

        let width = measure_default_size(width_measure_spec, w_min);
        let height = measure_default_size(height_measure_spec, h_min);

        static MEASURE_HONESTY_WARNED: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        if !MEASURE_HONESTY_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            tracing::warn!(
                target: "android.view.View",
                widget,
                width_measure_spec,
                height_measure_spec,
                width,
                height,
                "View.native_measure: serving installed getDefaultSize semantics — EXACTLY/AT_MOST \
                 → spec size (full parent budget), other modes → suggested minimum; text/image \
                 content is NOT measured headless, so AT_MOST children may be OVERSIZED and \
                 UNSPECIFIED-measured children may be UNDERSIZED (suggested minimum; ctor default \
                 0) relative to real content"
            );
        } else {
            tracing::trace!(
                target: "android.view.View",
                widget,
                width_measure_spec,
                height_measure_spec,
                width,
                height,
                "View.native_measure: served installed getDefaultSize semantics"
            );
        }

        if let Err(e) = env
            .call_method(
                &this,
                VIEW_SET_MEASURED_DIMENSION_NAME,
                VIEW_SET_MEASURED_DIMENSION_SIG,
                &[JValue::Int(width), JValue::Int(height)],
            )
            .and_then(|v| v.v())
        {
            if env.exception_check() {
                env.exception_describe();
                env.exception_clear();
            }
            tracing::warn!(
                target: "android.view.View",
                widget,
                width,
                height,
                error = %e,
                "View.native_measure: setMeasuredDimension upcall failed (described+cleared; \
                 measured fields stale until requestLayout or a spec change)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

fn suggested_minimum_if_needed(env: &mut Env, this: &JObject, spec: jint, name: &JNIStr) -> jint {
    let mode = spec & MEASURE_SPEC_MODE_MASK;
    if mode == MEASURE_SPEC_EXACTLY || mode == MEASURE_SPEC_AT_MOST {
        return 0;
    }
    match env
        .call_method(this, name, VIEW_GET_SUGGESTED_MIN_SIG, &[])
        .and_then(|v| v.i())
    {
        Ok(min) => min,
        Err(e) => {
            if env.exception_check() {
                env.exception_describe();
                env.exception_clear();
            }
            tracing::debug!(
                target: "android.view.View",
                name = %name.to_str(),
                error = %e,
                "View.native_measure: getSuggestedMinimum upcall failed (described+cleared; \
                 falling back to 0, the ctor default)"
            );
            0
        }
    }
}

extern "system" fn view_native_get_window<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
) -> JObject<'local> {
    env.with_env(|env| -> jni::errors::Result<JObject> {

        if let Err(e) = view_registry::with_view(widget, |_v| ()) {
            tracing::debug!(
                target: "android.view.View",
                widget,
                error = %e,
                "View.native_get_window: invalid view handle (returning the shared window anyway)"
            );
        }

        let active = window_registry::active_window();
        let local = match window_registry::with_jobject(active, |global| {
            env.new_local_ref(global.as_obj())
        }) {

            Ok(Some(Ok(obj))) => obj,

            Ok(Some(Err(e))) => return Err(e),

            Ok(None) => {
                tracing::debug!(
                    target: "android.view.Window",
                    active,
                    widget,
                    "View.native_get_window: no Window object captured yet → null (floating observer)"
                );
                JObject::null()
            }

            Err(e) => {
                tracing::debug!(
                    target: "android.view.Window",
                    active,
                    widget,
                    error = %e,
                    "View.native_get_window: no live process-shared window → null (floating observer)"
                );
                JObject::null()
            }
        };
        Ok(local)
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn view_native_destructor<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if has_webview_bridges() {
            drop_bridges_for(widget);
        }
        if let Err(error) = crate::webview::client::close_view(widget) {
            tracing::warn!(
                target: "android.webkit.WebView",
                widget,
                %error,
                "View.native_destructor: the finalized WebView's helper view was not closed"
            );
        }
        match view_registry::free(widget) {
            Ok(()) => tracing::debug!(
                target: "android.view.View",
                widget,
                "View.native_destructor: freed view-registry peer"
            ),

            Err(e) => tracing::debug!(
                target: "android.view.View",
                widget,
                error = %e,
                "View.native_destructor: no live peer for handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

type NativeBinding = (&'static JNIStr, &'static JNIStr, *mut c_void);

fn fold_best_effort(
    bindings: &[NativeBinding],
    mut step: impl FnMut(&NativeBinding) -> bool,
) -> u32 {
    let mut bound = 0u32;
    for binding in bindings {
        if step(binding) {
            bound += 1;
        }
    }
    bound
}

fn register_class_natives_best_effort(
    env: &mut Env,
    class_name: &JNIStr,
    bindings: &[NativeBinding],
) -> Result<u32, FrameworkError> {
    let class = env.find_class(class_name)?;
    let bound = fold_best_effort(bindings, |&(name, sig, ptr)| {
        let method = unsafe { NativeMethod::from_raw_parts(name, sig, ptr) };
        match unsafe { env.register_native_methods(&class, std::slice::from_ref(&method)) } {
            Ok(()) => true,
            Err(_) => {
                if env.exception_check() {
                    env.exception_clear();
                }
                tracing::warn!(
                    class = %class_name.to_str(),
                    method = %name.to_str(),
                    sig = %sig.to_str(),
                    "native not declared on this shipped framework class (skipped, best-effort) — \
                     will surface as a call-time UnsatisfiedLinkError if actually invoked"
                );
                false
            }
        }
    });
    Ok(bound)
}

fn register_view_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let bindings: [NativeBinding; 27] = [
        (
            VIEW_NATIVE_CONSTRUCTOR_NAME,
            VIEW_NATIVE_CONSTRUCTOR_SIG,
            view_native_constructor as *mut c_void,
        ),
        (
            VIEW_NATIVE_SET_PADDING_NAME,
            VIEW_NATIVE_SET_PADDING_SIG,
            view_native_set_padding as *mut c_void,
        ),
        (
            VIEW_NATIVE_SET_LAYOUT_PARAMS_NAME,
            VIEW_NATIVE_SET_LAYOUT_PARAMS_SIG,
            view_native_set_layout_params as *mut c_void,
        ),
        (
            VIEW_NATIVE_REQUEST_LAYOUT_NAME,
            VIEW_NATIVE_REQUEST_LAYOUT_SIG,
            view_native_request_layout as *mut c_void,
        ),
        (
            VIEW_NATIVE_SET_BACKGROUND_DRAWABLE_NAME,
            VIEW_NATIVE_SET_BACKGROUND_DRAWABLE_SIG,
            view_native_set_background_drawable as *mut c_void,
        ),
        (
            VIEW_NATIVE_SET_VISIBILITY_NAME,
            VIEW_NATIVE_SET_VISIBILITY_SIG,
            view_native_set_visibility as *mut c_void,
        ),
        (
            VIEW_SET_ON_CLICK_LISTENER_NAME,
            VIEW_SET_ON_CLICK_LISTENER_SIG,
            image_button_set_on_click_listener as *mut c_void,
        ),
        (
            VIEW_SET_ON_TOUCH_LISTENER_NAME,
            VIEW_SET_ON_TOUCH_LISTENER_SIG,
            view_set_input_listener as *mut c_void,
        ),
        (
            VIEW_SET_ON_LONG_CLICK_LISTENER_NAME,
            VIEW_SET_ON_LONG_CLICK_LISTENER_SIG,
            view_set_input_listener as *mut c_void,
        ),
        (
            VIEW_SET_BACKGROUND_COLOR_NAME,
            VIEW_SET_BACKGROUND_COLOR_SIG,
            view_native_set_background_color as *mut c_void,
        ),
        (
            VIEW_NATIVE_SET_FULLSCREEN_NAME,
            VIEW_NATIVE_SET_FULLSCREEN_SIG,
            view_native_set_fullscreen as *mut c_void,
        ),
        (
            VIEW_NATIVE_GET_WINDOW_NAME,
            VIEW_NATIVE_GET_WINDOW_SIG,
            view_native_get_window as *mut c_void,
        ),
        (
            VIEW_NATIVE_DESTRUCTOR_NAME,
            VIEW_NATIVE_DESTRUCTOR_SIG,
            view_native_destructor as *mut c_void,
        ),
        (
            VIEW_GET_WINDOW_VISIBLE_DISPLAY_FRAME_NAME,
            VIEW_GET_WINDOW_VISIBLE_DISPLAY_FRAME_SIG,
            view_get_window_visible_display_frame as *mut c_void,
        ),
        (
            VIEW_NATIVE_IS_ATTACHED_TO_WINDOW_NAME,
            VIEW_NATIVE_IS_ATTACHED_TO_WINDOW_SIG,
            view_native_is_attached_to_window as *mut c_void,
        ),
        (
            VIEW_NATIVE_GET_GLOBAL_VISIBLE_RECT_NAME,
            VIEW_NATIVE_GET_GLOBAL_VISIBLE_RECT_SIG,
            view_native_get_global_visible_rect as *mut c_void,
        ),
        (
            VIEW_NATIVE_REQUEST_FOCUS_NAME,
            VIEW_NATIVE_REQUEST_FOCUS_SIG,
            view_native_request_focus as *mut c_void,
        ),
        (
            VIEW_NATIVE_LAYOUT_NAME,
            VIEW_NATIVE_LAYOUT_SIG,
            view_native_layout as *mut c_void,
        ),
        (
            VIEW_NATIVE_IS_FOCUSED_NAME,
            VIEW_NATIVE_IS_FOCUSED_SIG,
            view_native_is_focused as *mut c_void,
        ),
        (
            VIEW_NATIVE_INVALIDATE_NAME,
            VIEW_NATIVE_INVALIDATE_SIG,
            view_native_invalidate as *mut c_void,
        ),
        (
            VIEW_NATIVE_KEEP_SCREEN_ON_NAME,
            VIEW_NATIVE_KEEP_SCREEN_ON_SIG,
            view_native_keep_screen_on as *mut c_void,
        ),
        (
            VIEW_NATIVE_ADD_CLASS_NAME,
            VIEW_NATIVE_ADD_CLASS_SIG,
            view_native_add_class as *mut c_void,
        ),
        (
            VIEW_NATIVE_REMOVE_CLASSES_NAME,
            VIEW_NATIVE_REMOVE_CLASSES_SIG,
            view_native_remove_classes as *mut c_void,
        ),
        (
            VIEW_NATIVE_DRAW_BACKGROUND_NAME,
            VIEW_NATIVE_DRAW_BACKGROUND_SIG,
            view_native_draw_background as *mut c_void,
        ),
        (
            VIEW_NATIVE_DRAW_CONTENT_NAME,
            VIEW_NATIVE_DRAW_CONTENT_SIG,
            view_native_draw_content as *mut c_void,
        ),
        (
            VIEW_NATIVE_QUEUE_ALLOCATE_NAME,
            VIEW_NATIVE_QUEUE_ALLOCATE_SIG,
            view_native_queue_allocate as *mut c_void,
        ),
        (
            VIEW_NATIVE_MEASURE_NAME,
            VIEW_NATIVE_MEASURE_SIG,
            view_native_measure as *mut c_void,
        ),
    ];

    let bound = register_class_natives_best_effort(env, VIEW_CLASS, &bindings)?;
    tracing::info!(
        class = "android/view/View",
        bound,
        "registered Eclipse's non-GTK backing for View.native_constructor + native_setPadding + native_setLayoutParams + native_requestLayout + native_setBackgroundDrawable + native_setVisibility + nativeSetOnClickListener + nativeSetOnTouchListener + nativeSetOnLongClickListener + native_setBackgroundColor + nativeSetFullscreen + native_get_window + native_destructor + getWindowVisibleDisplayFrame + nativeIsAttachedToWindow + native_getGlobalVisibleRect + nativeRequestFocus + native_layout + nativeIsFocused + nativeInvalidate + native_keep_screen_on + native_addClass + native_removeClasses + native_drawBackground + native_drawContent + native_queueAllocate + native_measure (native_addClasses/native_removeClass/native_getMatrix declaration-only dead code) (per-method best-effort)"
    );
    Ok(())
}

pub const VIEW_TREE_OBSERVER_CLASS: &JNIStr = jni_str!("android/view/ViewTreeObserver");

const VIEW_TREE_OBSERVER_SET_HAVE_LISTENERS_NAME: &JNIStr =
    jni_str!("native_set_have_global_layout_listeners");
const VIEW_TREE_OBSERVER_SET_HAVE_LISTENERS_SIG: &JNIStr = jni_str!("(Z)V");
const MAX_GLOBAL_LAYOUT_OBSERVERS: usize = 16;

struct GlobalLayoutObserver {
    jobject: Global<JObject<'static>>,
    pending: bool,
}

static GLOBAL_LAYOUT_OBSERVERS: std::sync::Mutex<Vec<GlobalLayoutObserver>> =
    std::sync::Mutex::new(Vec::new());

fn update_global_layout_observer(
    env: &Env,
    observer: &JObject,
    have_listeners: bool,
) -> jni::errors::Result<()> {
    let mut observers = GLOBAL_LAYOUT_OBSERVERS.lock().map_err(|error| {
        tracing::error!(
            target: "android.view.ViewTreeObserver",
            %error,
            "global-layout observer registry poisoned"
        );
        jni::errors::Error::JniCall(jni::errors::JniError::Unknown)
    })?;

    let mut matching_index = None;
    for (index, entry) in observers.iter().enumerate() {
        if env.is_same_object(entry.jobject.as_obj(), observer)? {
            matching_index = Some(index);
            break;
        }
    }

    if have_listeners {
        if let Some(index) = matching_index {
            observers[index].pending = true;
            return Ok(());
        }
        if observers.len() == MAX_GLOBAL_LAYOUT_OBSERVERS {
            tracing::error!(
                target: "android.view.ViewTreeObserver",
                limit = MAX_GLOBAL_LAYOUT_OBSERVERS,
                "global-layout observer limit reached"
            );
            return Err(jni::errors::Error::JniCall(jni::errors::JniError::Unknown));
        }
        observers.push(GlobalLayoutObserver {
            jobject: env.new_global_ref(observer)?,
            pending: true,
        });
    } else if let Some(index) = matching_index {
        observers.remove(index);
    }
    Ok(())
}

fn mark_global_layout_pending() {
    match GLOBAL_LAYOUT_OBSERVERS.lock() {
        Ok(mut observers) => {
            for observer in observers.iter_mut() {
                observer.pending = true;
            }
        }
        Err(error) => tracing::error!(
            target: "android.view.ViewTreeObserver",
            %error,
            "global-layout observer registry poisoned while scheduling layout"
        ),
    }
}

pub fn global_layout_pending() -> bool {
    GLOBAL_LAYOUT_OBSERVERS
        .lock()
        .is_ok_and(|observers| observers.iter().any(|observer| observer.pending))
}

fn dispatch_pending_global_layout(env: &mut Env) -> Result<(), FrameworkError> {
    let pending = {
        let mut observers = GLOBAL_LAYOUT_OBSERVERS
            .lock()
            .map_err(|_| FrameworkError::GlobalLayoutObserverRegistryPoisoned)?;
        let mut pending = Vec::with_capacity(observers.len());
        for observer in observers.iter_mut().filter(|observer| observer.pending) {
            pending.push(checked(env, "ViewTreeObserver NewLocalRef", |env| {
                env.new_local_ref(observer.jobject.as_obj())
            })?);
            observer.pending = false;
        }
        pending
    };

    for observer in pending {
        checked(env, "ViewTreeObserver.dispatchOnGlobalLayout", |env| {
            env.call_method(
                &observer,
                jni_str!("dispatchOnGlobalLayout"),
                jni_sig!("()V"),
                &[],
            )?
            .v()
        })?;
    }
    Ok(())
}

extern "system" fn view_tree_observer_set_have_global_layout_listeners<'local>(
    mut env: EnvUnowned<'local>,
    this: JObject<'local>,
    have_listeners: jboolean,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        update_global_layout_observer(env, &this, have_listeners)?;
        tracing::trace!(
            target: "android.view.ViewTreeObserver",
            have_listeners,
            "ViewTreeObserver.native_set_have_global_layout_listeners: updated bounded host dispatch registration"
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

fn register_view_tree_observer_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(VIEW_TREE_OBSERVER_CLASS)?;
    let methods = [unsafe {
        NativeMethod::from_raw_parts(
            VIEW_TREE_OBSERVER_SET_HAVE_LISTENERS_NAME,
            VIEW_TREE_OBSERVER_SET_HAVE_LISTENERS_SIG,
            view_tree_observer_set_have_global_layout_listeners as *mut std::ffi::c_void,
        )
    }];

    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/view/ViewTreeObserver",
        "registered Eclipse's backing for ViewTreeObserver.native_set_have_global_layout_listeners"
    );
    Ok(())
}

pub const VIEW_GROUP_CLASS: &JNIStr = jni_str!("android/view/ViewGroup");

const VIEW_GROUP_NATIVE_ADD_VIEW_NAME: &JNIStr = jni_str!("native_addView");
const VIEW_GROUP_NATIVE_ADD_VIEW_SIG: &JNIStr =
    jni_str!("(JJILandroid/view/ViewGroup$LayoutParams;)V");

const VIEW_GROUP_NATIVE_REMOVE_VIEW_NAME: &JNIStr = jni_str!("native_removeView");
const VIEW_GROUP_NATIVE_REMOVE_VIEW_SIG: &JNIStr = jni_str!("(JJ)V");

extern "system" fn view_group_native_add_view<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    parent: jlong,
    child: jlong,
    index: jint,
    _params: JObject<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        match view_registry::attach_child(parent, child, usize::try_from(index).ok()) {
            Ok(()) => tracing::debug!(
                target: "android.view.ViewGroup",
                parent,
                child,
                index,
                "ViewGroup.native_addView: recorded parent→child tree edge (non-GTK)"
            ),
            Err(e) => tracing::debug!(
                target: "android.view.ViewGroup",
                parent,
                child,
                error = %e,
                "ViewGroup.native_addView: invalid parent or child handle (ignored)"
            ),
        }
        crate::webview::client::refresh_visibility(env);
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn view_group_native_remove_view<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    parent: jlong,
    child: jlong,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        match view_registry::detach_child(parent, child) {
            Ok(()) => tracing::debug!(
                target: "android.view.ViewGroup",
                parent,
                child,
                "ViewGroup.native_removeView: removed parent→child tree edge (non-GTK)"
            ),
            Err(e) => tracing::debug!(
                target: "android.view.ViewGroup",
                parent,
                child,
                error = %e,
                "ViewGroup.native_removeView: invalid parent handle (ignored)"
            ),
        }
        crate::webview::client::refresh_visibility(env);
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

fn register_view_group_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let bindings: [NativeBinding; 2] = [
        (
            VIEW_GROUP_NATIVE_ADD_VIEW_NAME,
            VIEW_GROUP_NATIVE_ADD_VIEW_SIG,
            view_group_native_add_view as *mut c_void,
        ),
        (
            VIEW_GROUP_NATIVE_REMOVE_VIEW_NAME,
            VIEW_GROUP_NATIVE_REMOVE_VIEW_SIG,
            view_group_native_remove_view as *mut c_void,
        ),
    ];
    let bound = register_class_natives_best_effort(env, VIEW_GROUP_CLASS, &bindings)?;
    tracing::info!(
        class = "android/view/ViewGroup",
        bound,
        "registered Eclipse's non-GTK backing for ViewGroup.native_addView + native_removeView (per-method best-effort)"
    );
    Ok(())
}

pub const PAINT_CLASS: &JNIStr = jni_str!("android/graphics/Paint");

const PAINT_NATIVE_CREATE_NAME: &JNIStr = jni_str!("native_create");
const PAINT_NATIVE_CREATE_SIG: &JNIStr = jni_str!("()J");

const PAINT_NATIVE_SET_COLOR_NAME: &JNIStr = jni_str!("native_set_color");
const PAINT_NATIVE_SET_COLOR_SIG: &JNIStr = jni_str!("(JI)V");

const PAINT_NATIVE_SET_STROKE_WIDTH_NAME: &JNIStr = jni_str!("native_set_stroke_width");
const PAINT_NATIVE_SET_STROKE_WIDTH_SIG: &JNIStr = jni_str!("(JF)V");

const PAINT_NATIVE_SET_STYLE_NAME: &JNIStr = jni_str!("native_set_style");
const PAINT_NATIVE_SET_STYLE_SIG: &JNIStr = jni_str!("(JI)V");

const PAINT_NATIVE_SET_TEXT_SIZE_NAME: &JNIStr = jni_str!("native_set_text_size");
const PAINT_NATIVE_SET_TEXT_SIZE_SIG: &JNIStr = jni_str!("(JF)V");

const PAINT_NATIVE_CLONE_NAME: &JNIStr = jni_str!("native_clone");
const PAINT_NATIVE_CLONE_SIG: &JNIStr = jni_str!("(J)J");
const PAINT_NATIVE_RECYCLE_NAME: &JNIStr = jni_str!("native_recycle");
const PAINT_NATIVE_RECYCLE_SIG: &JNIStr = jni_str!("(J)V");
const PAINT_NATIVE_GET_COLOR_NAME: &JNIStr = jni_str!("native_get_color");
const PAINT_NATIVE_GET_COLOR_SIG: &JNIStr = jni_str!("(J)I");
const PAINT_NATIVE_SET_ALPHA_NAME: &JNIStr = jni_str!("native_set_alpha");
const PAINT_NATIVE_SET_ALPHA_SIG: &JNIStr = jni_str!("(JI)V");
const PAINT_NATIVE_GET_ALPHA_NAME: &JNIStr = jni_str!("native_get_alpha");
const PAINT_NATIVE_GET_ALPHA_SIG: &JNIStr = jni_str!("(J)I");
const PAINT_NATIVE_GET_STYLE_NAME: &JNIStr = jni_str!("native_get_style");
const PAINT_NATIVE_GET_STYLE_SIG: &JNIStr = jni_str!("(J)I");
const PAINT_NATIVE_GET_STROKE_WIDTH_NAME: &JNIStr = jni_str!("native_get_stroke_width");
const PAINT_NATIVE_GET_STROKE_WIDTH_SIG: &JNIStr = jni_str!("(J)F");
const PAINT_NATIVE_SET_STROKE_CAP_NAME: &JNIStr = jni_str!("native_set_stroke_cap");
const PAINT_NATIVE_SET_STROKE_CAP_SIG: &JNIStr = jni_str!("(JI)V");
const PAINT_NATIVE_GET_STROKE_CAP_NAME: &JNIStr = jni_str!("native_get_stroke_cap");
const PAINT_NATIVE_GET_STROKE_CAP_SIG: &JNIStr = jni_str!("(J)I");
const PAINT_NATIVE_SET_STROKE_JOIN_NAME: &JNIStr = jni_str!("native_set_stroke_join");
const PAINT_NATIVE_SET_STROKE_JOIN_SIG: &JNIStr = jni_str!("(JI)V");
const PAINT_NATIVE_GET_STROKE_JOIN_NAME: &JNIStr = jni_str!("native_get_stroke_join");
const PAINT_NATIVE_GET_STROKE_JOIN_SIG: &JNIStr = jni_str!("(J)I");
const PAINT_NATIVE_GET_TEXT_SIZE_NAME: &JNIStr = jni_str!("native_get_text_size");
const PAINT_NATIVE_GET_TEXT_SIZE_SIG: &JNIStr = jni_str!("(J)F");
const PAINT_NATIVE_SET_COLOR_FILTER_NAME: &JNIStr = jni_str!("native_set_color_filter");
const PAINT_NATIVE_SET_COLOR_FILTER_SIG: &JNIStr = jni_str!("(JII)V");
const PAINT_NATIVE_SET_TEXT_ALIGN_NAME: &JNIStr = jni_str!("native_set_text_align");
const PAINT_NATIVE_SET_TEXT_ALIGN_SIG: &JNIStr = jni_str!("(JI)V");

extern "system" fn paint_native_create<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        match paint_registry::allocate() {
            Ok(handle) => {
                tracing::debug!(
                    target: "android.graphics.Paint",
                    handle,
                    "Paint.native_create: allocated non-GTK paint-registry handle"
                );
                Ok(handle)
            }
            Err(e) => {
                tracing::warn!(
                    target: "android.graphics.Paint",
                    error = %e,
                    "Paint.native_create: paint-registry allocate failed → 0 (no paint)"
                );
                Ok(0)
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn paint_native_set_color<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    native_paint: jlong,
    color: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = paint_registry::with_paint(native_paint, |p| p.color = color) {
            tracing::debug!(
                target: "android.graphics.Paint",
                native_paint,
                error = %e,
                "Paint.native_set_color: invalid paint handle (ignored)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn paint_native_set_stroke_width<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    native_paint: jlong,
    width: f32,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = paint_registry::with_paint(native_paint, |p| p.stroke_width = width) {
            tracing::debug!(
                target: "android.graphics.Paint",
                native_paint,
                error = %e,
                "Paint.native_set_stroke_width: invalid paint handle (ignored)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn paint_native_set_style<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    native_paint: jlong,
    style: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        let resolved = paint_registry::PaintStyle::from_ordinal(style);
        if let Err(e) = paint_registry::with_paint(native_paint, |p| p.style = resolved) {
            tracing::debug!(
                target: "android.graphics.Paint",
                native_paint,
                style,
                error = %e,
                "Paint.native_set_style: invalid paint handle (ignored)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn paint_native_set_text_size<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    native_paint: jlong,
    size: f32,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = paint_registry::with_paint(native_paint, |p| p.text_size = size) {
            tracing::debug!(
                target: "android.graphics.Paint",
                native_paint,
                error = %e,
                "Paint.native_set_text_size: invalid paint handle (ignored)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

fn paint_color_with_alpha(color: jint, alpha: jint) -> jint {
    (color & 0x00FF_FFFF) | ((alpha & 0xFF) << 24)
}

extern "system" fn paint_native_clone<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    src: jlong,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        match paint_registry::clone_of(src) {
            Ok(handle) => {
                tracing::trace!(
                    target: "android.graphics.Paint",
                    src,
                    handle,
                    "Paint.native_clone: cloned paint-registry state"
                );
                Ok(handle)
            }
            Err(e) => {
                tracing::warn!(
                    target: "android.graphics.Paint",
                    src,
                    error = %e,
                    "Paint.native_clone: dead source handle → fresh default paint"
                );
                Ok(paint_registry::allocate().unwrap_or(0))
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn paint_native_recycle<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    native_paint: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if native_paint == 0 {
            return Ok(());
        }
        match paint_registry::free(native_paint) {
            Ok(()) => tracing::trace!(
                target: "android.graphics.Paint",
                native_paint,
                "Paint.native_recycle: freed recorded paint"
            ),
            Err(e) => tracing::debug!(
                target: "android.graphics.Paint",
                native_paint,
                error = %e,
                "Paint.native_recycle: dead handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn paint_native_get_color<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    native_paint: jlong,
) -> jint {
    env.with_env(|_env| -> jni::errors::Result<jint> {
        Ok(
            paint_registry::with_paint(native_paint, |p| p.color).unwrap_or_else(|e| {
                tracing::debug!(
                    target: "android.graphics.Paint",
                    native_paint,
                    error = %e,
                    "Paint.native_get_color: invalid paint handle → default (opaque black)"
                );
                paint_registry::PaintState::default().color
            }),
        )
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn paint_native_set_alpha<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    native_paint: jlong,
    alpha: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = paint_registry::with_paint(native_paint, |p| {
            p.color = paint_color_with_alpha(p.color, alpha);
        }) {
            tracing::debug!(
                target: "android.graphics.Paint",
                native_paint,
                error = %e,
                "Paint.native_set_alpha: invalid paint handle (ignored)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn paint_native_get_alpha<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    native_paint: jlong,
) -> jint {
    env.with_env(|_env| -> jni::errors::Result<jint> {
        Ok(
            paint_registry::with_paint(native_paint, |p| (p.color >> 24) & 0xFF).unwrap_or_else(
                |e| {
                    tracing::debug!(
                        target: "android.graphics.Paint",
                        native_paint,
                        error = %e,
                        "Paint.native_get_alpha: invalid paint handle → 255 (opaque default)"
                    );
                    0xFF
                },
            ),
        )
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn paint_native_get_style<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    native_paint: jlong,
) -> jint {
    env.with_env(|_env| -> jni::errors::Result<jint> {
        Ok(
            paint_registry::with_paint(native_paint, |p| p.style.ordinal()).unwrap_or_else(|e| {
                tracing::debug!(
                    target: "android.graphics.Paint",
                    native_paint,
                    error = %e,
                    "Paint.native_get_style: invalid paint handle → 0 (FILL)"
                );
                0
            }),
        )
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn paint_native_get_stroke_width<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    native_paint: jlong,
) -> jfloat {
    env.with_env(|_env| -> jni::errors::Result<jfloat> {
        Ok(
            paint_registry::with_paint(native_paint, |p| p.stroke_width).unwrap_or_else(|e| {
                tracing::debug!(
                    target: "android.graphics.Paint",
                    native_paint,
                    error = %e,
                    "Paint.native_get_stroke_width: invalid paint handle → 0.0"
                );
                0.0
            }),
        )
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn paint_native_set_stroke_cap<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    native_paint: jlong,
    cap: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        let resolved = paint_registry::StrokeCap::from_ordinal(cap);
        if let Err(e) = paint_registry::with_paint(native_paint, |p| p.stroke_cap = resolved) {
            tracing::debug!(
                target: "android.graphics.Paint",
                native_paint,
                cap,
                error = %e,
                "Paint.native_set_stroke_cap: invalid paint handle (ignored)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn paint_native_get_stroke_cap<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    native_paint: jlong,
) -> jint {
    env.with_env(|_env| -> jni::errors::Result<jint> {
        Ok(
            paint_registry::with_paint(native_paint, |p| p.stroke_cap.ordinal()).unwrap_or_else(
                |e| {
                    tracing::debug!(
                        target: "android.graphics.Paint",
                        native_paint,
                        error = %e,
                        "Paint.native_get_stroke_cap: invalid paint handle → 0 (BUTT)"
                    );
                    0
                },
            ),
        )
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn paint_native_set_stroke_join<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    native_paint: jlong,
    join: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        let resolved = paint_registry::StrokeJoin::from_ordinal(join);
        if let Err(e) = paint_registry::with_paint(native_paint, |p| p.stroke_join = resolved) {
            tracing::debug!(
                target: "android.graphics.Paint",
                native_paint,
                join,
                error = %e,
                "Paint.native_set_stroke_join: invalid paint handle (ignored)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn paint_native_get_stroke_join<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    native_paint: jlong,
) -> jint {
    env.with_env(|_env| -> jni::errors::Result<jint> {
        Ok(
            paint_registry::with_paint(native_paint, |p| p.stroke_join.ordinal()).unwrap_or_else(
                |e| {
                    tracing::debug!(
                        target: "android.graphics.Paint",
                        native_paint,
                        error = %e,
                        "Paint.native_get_stroke_join: invalid paint handle → 0 (MITER)"
                    );
                    0
                },
            ),
        )
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn paint_native_get_text_size<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    native_paint: jlong,
) -> jfloat {
    env.with_env(|_env| -> jni::errors::Result<jfloat> {
        Ok(
            paint_registry::with_paint(native_paint, |p| p.text_size).unwrap_or_else(|e| {
                tracing::debug!(
                    target: "android.graphics.Paint",
                    native_paint,
                    error = %e,
                    "Paint.native_get_text_size: invalid paint handle → 0.0"
                );
                0.0
            }),
        )
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn paint_native_set_color_filter<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    native_paint: jlong,
    mode: jint,
    color: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        tracing::trace!(
            target: "android.graphics.Paint",
            native_paint,
            mode,
            color,
            "Paint.native_set_color_filter: no-op (headless recording; Java retains the ColorFilter)"
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn paint_native_set_text_align<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    native_paint: jlong,
    align: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        tracing::trace!(
            target: "android.graphics.Paint",
            native_paint,
            align,
            "Paint.native_set_text_align: no-op (headless recording; Java retains the Align)"
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

fn register_paint_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let bindings: [NativeBinding; 19] = [
        (
            PAINT_NATIVE_CREATE_NAME,
            PAINT_NATIVE_CREATE_SIG,
            paint_native_create as *mut c_void,
        ),
        (
            PAINT_NATIVE_CLONE_NAME,
            PAINT_NATIVE_CLONE_SIG,
            paint_native_clone as *mut c_void,
        ),
        (
            PAINT_NATIVE_RECYCLE_NAME,
            PAINT_NATIVE_RECYCLE_SIG,
            paint_native_recycle as *mut c_void,
        ),
        (
            PAINT_NATIVE_SET_COLOR_NAME,
            PAINT_NATIVE_SET_COLOR_SIG,
            paint_native_set_color as *mut c_void,
        ),
        (
            PAINT_NATIVE_GET_COLOR_NAME,
            PAINT_NATIVE_GET_COLOR_SIG,
            paint_native_get_color as *mut c_void,
        ),
        (
            PAINT_NATIVE_SET_ALPHA_NAME,
            PAINT_NATIVE_SET_ALPHA_SIG,
            paint_native_set_alpha as *mut c_void,
        ),
        (
            PAINT_NATIVE_GET_ALPHA_NAME,
            PAINT_NATIVE_GET_ALPHA_SIG,
            paint_native_get_alpha as *mut c_void,
        ),
        (
            PAINT_NATIVE_SET_STYLE_NAME,
            PAINT_NATIVE_SET_STYLE_SIG,
            paint_native_set_style as *mut c_void,
        ),
        (
            PAINT_NATIVE_GET_STYLE_NAME,
            PAINT_NATIVE_GET_STYLE_SIG,
            paint_native_get_style as *mut c_void,
        ),
        (
            PAINT_NATIVE_SET_STROKE_WIDTH_NAME,
            PAINT_NATIVE_SET_STROKE_WIDTH_SIG,
            paint_native_set_stroke_width as *mut c_void,
        ),
        (
            PAINT_NATIVE_GET_STROKE_WIDTH_NAME,
            PAINT_NATIVE_GET_STROKE_WIDTH_SIG,
            paint_native_get_stroke_width as *mut c_void,
        ),
        (
            PAINT_NATIVE_SET_STROKE_CAP_NAME,
            PAINT_NATIVE_SET_STROKE_CAP_SIG,
            paint_native_set_stroke_cap as *mut c_void,
        ),
        (
            PAINT_NATIVE_GET_STROKE_CAP_NAME,
            PAINT_NATIVE_GET_STROKE_CAP_SIG,
            paint_native_get_stroke_cap as *mut c_void,
        ),
        (
            PAINT_NATIVE_SET_STROKE_JOIN_NAME,
            PAINT_NATIVE_SET_STROKE_JOIN_SIG,
            paint_native_set_stroke_join as *mut c_void,
        ),
        (
            PAINT_NATIVE_GET_STROKE_JOIN_NAME,
            PAINT_NATIVE_GET_STROKE_JOIN_SIG,
            paint_native_get_stroke_join as *mut c_void,
        ),
        (
            PAINT_NATIVE_SET_TEXT_SIZE_NAME,
            PAINT_NATIVE_SET_TEXT_SIZE_SIG,
            paint_native_set_text_size as *mut c_void,
        ),
        (
            PAINT_NATIVE_GET_TEXT_SIZE_NAME,
            PAINT_NATIVE_GET_TEXT_SIZE_SIG,
            paint_native_get_text_size as *mut c_void,
        ),
        (
            PAINT_NATIVE_SET_COLOR_FILTER_NAME,
            PAINT_NATIVE_SET_COLOR_FILTER_SIG,
            paint_native_set_color_filter as *mut c_void,
        ),
        (
            PAINT_NATIVE_SET_TEXT_ALIGN_NAME,
            PAINT_NATIVE_SET_TEXT_ALIGN_SIG,
            paint_native_set_text_align as *mut c_void,
        ),
    ];
    let bound = register_class_natives_best_effort(env, PAINT_CLASS, &bindings)?;
    tracing::info!(
        bound,
        "registered Eclipse's non-GTK backing for the android.graphics.Paint native surface (create/clone/recycle + color/alpha/style/stroke-width/stroke-cap/stroke-join/text-size get+set + color-filter/text-align no-ops; native_get_text_bounds deliberately unbound — real text metrics) (per-method best-effort)"
    );
    Ok(())
}

pub const MATRIX_CLASS: &JNIStr = jni_str!("android/graphics/Matrix");

const MATRIX_NATIVE_CREATE_NAME: &JNIStr = jni_str!("native_create");
const MATRIX_NATIVE_CREATE_SIG: &JNIStr = jni_str!("(J)J");

const MATRIX_FINALIZER_NAME: &JNIStr = jni_str!("finalizer");
const MATRIX_FINALIZER_SIG: &JNIStr = jni_str!("(J)V");

extern "system" fn matrix_native_create<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    src: jlong,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        let value = match matrix_registry::get(src) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    target: "android.graphics.Matrix",
                    src,
                    error = %e,
                    "Matrix.native_create: source handle invalid → identity"
                );
                matrix_registry::Affine::IDENTITY
            }
        };
        match matrix_registry::allocate(value) {
            Ok(handle) => {
                tracing::debug!(
                    target: "android.graphics.Matrix",
                    src,
                    handle,
                    "Matrix.native_create: allocated non-GTK matrix-registry handle"
                );
                Ok(handle)
            }
            Err(e) => {
                tracing::warn!(
                    target: "android.graphics.Matrix",
                    error = %e,
                    "Matrix.native_create: matrix-registry allocate failed → 0 (identity)"
                );
                Ok(0)
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn matrix_finalizer<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    native_instance: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = matrix_registry::free(native_instance) {
            tracing::debug!(
                target: "android.graphics.Matrix",
                native_instance,
                error = %e,
                "Matrix.finalizer: handle already freed / identity sentinel (ignored)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

fn register_matrix_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(MATRIX_CLASS)?;
    let methods = [
        unsafe {
            NativeMethod::from_raw_parts(
                MATRIX_NATIVE_CREATE_NAME,
                MATRIX_NATIVE_CREATE_SIG,
                matrix_native_create as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                MATRIX_FINALIZER_NAME,
                MATRIX_FINALIZER_SIG,
                matrix_finalizer as *mut std::ffi::c_void,
            )
        },
    ];

    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/graphics/Matrix",
        "registered Eclipse's non-GTK backing for Matrix.native_create + finalizer"
    );
    Ok(())
}

pub const PATH_CLASS: &JNIStr = jni_str!("android/graphics/Path");

const PATH_NATIVE_CREATE_BUILDER_NAME: &JNIStr = jni_str!("native_create_builder");
const PATH_NATIVE_CREATE_BUILDER_SIG: &JNIStr = jni_str!("(JJ)J");

const PATH_NATIVE_MOVE_TO_NAME: &JNIStr = jni_str!("native_move_to");
const PATH_NATIVE_MOVE_TO_SIG: &JNIStr = jni_str!("(JFF)V");
const PATH_NATIVE_LINE_TO_NAME: &JNIStr = jni_str!("native_line_to");
const PATH_NATIVE_LINE_TO_SIG: &JNIStr = jni_str!("(JFF)V");
const PATH_NATIVE_QUAD_TO_NAME: &JNIStr = jni_str!("native_quad_to");
const PATH_NATIVE_QUAD_TO_SIG: &JNIStr = jni_str!("(JFFFF)V");
const PATH_NATIVE_CUBIC_TO_NAME: &JNIStr = jni_str!("native_cubic_to");
const PATH_NATIVE_CUBIC_TO_SIG: &JNIStr = jni_str!("(JFFFFFF)V");
const PATH_NATIVE_CLOSE_NAME: &JNIStr = jni_str!("native_close");
const PATH_NATIVE_CLOSE_SIG: &JNIStr = jni_str!("(J)V");

const PATH_NATIVE_CREATE_PATH_NAME: &JNIStr = jni_str!("native_create_path");
const PATH_NATIVE_CREATE_PATH_SIG: &JNIStr = jni_str!("(J)J");

const PATH_NATIVE_REF_PATH_NAME: &JNIStr = jni_str!("native_ref_path");
const PATH_NATIVE_REF_PATH_SIG: &JNIStr = jni_str!("(J)J");

const PATH_NATIVE_RESET_NAME: &JNIStr = jni_str!("native_reset");
const PATH_NATIVE_RESET_SIG: &JNIStr = jni_str!("(JJ)V");

extern "system" fn path_native_create_builder<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    native_path: jlong,
    builder: jlong,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        Ok(path_rebuild_builder(native_path, builder))
    })
    .resolve::<LogErrorAndDefault>()
}

fn path_record(
    handle: jlong,
    op_name: &'static str,
    op: impl FnOnce(&mut path_registry::PathGeometry),
) {
    match path_registry::with_path(handle, op) {
        Ok(()) => {
            tracing::trace!(
                target: "android.graphics.Path",
                handle,
                op = op_name,
                "Path builder op recorded on path-registry geometry"
            );
        }
        Err(e) => {
            tracing::warn!(
                target: "android.graphics.Path",
                handle,
                op = op_name,
                error = %e,
                "Path builder op: builder handle invalid (ignored)"
            );
        }
    }
}

extern "system" fn path_native_move_to<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    builder: jlong,
    x: jfloat,
    y: jfloat,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        path_record(builder, "moveTo", |g| g.move_to(x, y));
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn path_native_line_to<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    builder: jlong,
    x: jfloat,
    y: jfloat,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        path_record(builder, "lineTo", |g| g.line_to(x, y));
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn path_native_quad_to<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    builder: jlong,
    cx: jfloat,
    cy: jfloat,
    x: jfloat,
    y: jfloat,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        path_record(builder, "quadTo", |g| g.quad_to(cx, cy, x, y));
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn path_native_cubic_to<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    builder: jlong,
    c1x: jfloat,
    c1y: jfloat,
    c2x: jfloat,
    c2y: jfloat,
    x: jfloat,
    y: jfloat,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        path_record(builder, "cubicTo", |g| g.cubic_to(c1x, c1y, c2x, c2y, x, y));
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn path_native_close<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    builder: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        path_record(builder, "close", |g| g.close());
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

fn path_clone_handle(source: jlong, op_name: &'static str) -> jlong {
    let geometry = if source == 0 {
        path_registry::PathGeometry::default()
    } else {
        match path_registry::get(source) {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(
                    target: "android.graphics.Path",
                    source,
                    op = op_name,
                    error = %e,
                    "Path clone: source handle invalid → empty path"
                );
                path_registry::PathGeometry::default()
            }
        }
    };
    path_allocate(geometry, op_name)
}

fn path_allocate(geometry: path_registry::PathGeometry, op_name: &'static str) -> jlong {
    match path_registry::allocate(geometry) {
        Ok(handle) => {
            tracing::debug!(
                target: "android.graphics.Path",
                handle,
                op = op_name,
                "Path: allocated independently-owned path-registry geometry"
            );
            handle
        }
        Err(e) => {
            tracing::warn!(
                target: "android.graphics.Path",
                op = op_name,
                error = %e,
                "Path: path-registry allocate failed → 0 (empty path)"
            );
            0
        }
    }
}

fn path_rebuild_builder(native_path: jlong, builder: jlong) -> jlong {
    let mut geometry = if native_path == 0 {
        path_registry::PathGeometry::default()
    } else {
        path_registry::take(native_path).unwrap_or_else(|e| {
            tracing::warn!(
                target: "android.graphics.Path",
                native_path,
                error = %e,
                "Path.native_create_builder: consumed path handle invalid → empty path"
            );
            path_registry::PathGeometry::default()
        })
    };
    if builder == 0 {
        return path_allocate(geometry, "native_create_builder");
    }
    match path_registry::with_path(builder, |g| std::mem::swap(g, &mut geometry)) {
        Ok(()) => builder,
        Err(e) => {
            tracing::warn!(
                target: "android.graphics.Path",
                builder,
                error = %e,
                "Path.native_create_builder: builder handle invalid → new builder"
            );
            path_allocate(geometry, "native_create_builder")
        }
    }
}

extern "system" fn path_native_create_path<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    builder: jlong,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        Ok(path_clone_handle(builder, "native_create_path"))
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn path_native_ref_path<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    src: jlong,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        Ok(path_clone_handle(src, "native_ref_path"))
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn path_native_reset<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    path: jlong,
    builder: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        for (handle, role) in [(path, "path"), (builder, "builder")] {
            if handle == 0 {
                continue;
            }
            if let Err(e) = path_registry::free(handle) {
                tracing::debug!(
                    target: "android.graphics.Path",
                    handle,
                    role,
                    error = %e,
                    "Path.native_reset: invalid handle (ignored)"
                );
            }
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

fn register_path_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(PATH_CLASS)?;
    let methods = [
        unsafe {
            NativeMethod::from_raw_parts(
                PATH_NATIVE_CREATE_BUILDER_NAME,
                PATH_NATIVE_CREATE_BUILDER_SIG,
                path_native_create_builder as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                PATH_NATIVE_MOVE_TO_NAME,
                PATH_NATIVE_MOVE_TO_SIG,
                path_native_move_to as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                PATH_NATIVE_LINE_TO_NAME,
                PATH_NATIVE_LINE_TO_SIG,
                path_native_line_to as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                PATH_NATIVE_QUAD_TO_NAME,
                PATH_NATIVE_QUAD_TO_SIG,
                path_native_quad_to as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                PATH_NATIVE_CUBIC_TO_NAME,
                PATH_NATIVE_CUBIC_TO_SIG,
                path_native_cubic_to as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                PATH_NATIVE_CLOSE_NAME,
                PATH_NATIVE_CLOSE_SIG,
                path_native_close as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                PATH_NATIVE_CREATE_PATH_NAME,
                PATH_NATIVE_CREATE_PATH_SIG,
                path_native_create_path as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                PATH_NATIVE_REF_PATH_NAME,
                PATH_NATIVE_REF_PATH_SIG,
                path_native_ref_path as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                PATH_NATIVE_RESET_NAME,
                PATH_NATIVE_RESET_SIG,
                path_native_reset as *mut std::ffi::c_void,
            )
        },
    ];

    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/graphics/Path",
        "registered Eclipse's non-GTK backing for Path.native_create_builder + move/line/quad/cubic/close + create_path/ref_path/reset"
    );
    Ok(())
}

pub const CANVAS_CLASS: &JNIStr = jni_str!("android/graphics/Canvas");

const CANVAS_N_DRAW_COLOR_NAME: &JNIStr = jni_str!("nDrawColor");
const CANVAS_N_DRAW_COLOR_SIG: MethodSignature<'static, 'static> = jni_sig!("(JI)V");
const CANVAS_N_DRAW_RECT_NAME: &JNIStr = jni_str!("nDrawRect");
const CANVAS_N_DRAW_RECT_SIG: MethodSignature<'static, 'static> = jni_sig!("(JFFFFJ)V");
const CANVAS_N_DRAW_CIRCLE_NAME: &JNIStr = jni_str!("nDrawCircle");
const CANVAS_N_DRAW_CIRCLE_SIG: MethodSignature<'static, 'static> = jni_sig!("(JFFFJ)V");
const CANVAS_N_DRAW_PATH_NAME: &JNIStr = jni_str!("nDrawPath");
const CANVAS_N_DRAW_PATH_SIG: MethodSignature<'static, 'static> = jni_sig!("(JJJ)V");

fn paint_config_from_handle(paint: jlong) -> canvas_registry::PaintConfig {
    paint_registry::with_paint(paint, |p| canvas_registry::PaintConfig {
        argb: p.color,
        style: p.style,
        stroke_width: p.stroke_width,

        even_odd: false,
    })
    .unwrap_or_default()
}

extern "system" fn canvas_n_draw_color<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    canvas: jlong,
    color: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        match canvas_registry::with_canvas(canvas, |c| c.draw_color(color)) {
            Ok(()) => tracing::trace!(
                target: "android.graphics.Canvas",
                canvas, color,
                "Canvas.nDrawColor: filled the Pixmap (real tiny-skia)"
            ),
            Err(e) => tracing::debug!(
                target: "android.graphics.Canvas",
                canvas, error = %e,
                "Canvas.nDrawColor: invalid canvas handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn canvas_n_draw_rect<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    canvas: jlong,
    left: jfloat,
    top: jfloat,
    right: jfloat,
    bottom: jfloat,
    paint: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        let cfg = paint_config_from_handle(paint);
        match canvas_registry::with_canvas(canvas, |c| c.draw_rect(left, top, right, bottom, &cfg))
        {
            Ok(()) => tracing::trace!(
                target: "android.graphics.Canvas",
                canvas, left, top, right, bottom,
                "Canvas.nDrawRect: rasterized a rect (real tiny-skia)"
            ),
            Err(e) => tracing::debug!(
                target: "android.graphics.Canvas",
                canvas, error = %e,
                "Canvas.nDrawRect: invalid canvas handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn canvas_n_draw_circle<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    canvas: jlong,
    cx: jfloat,
    cy: jfloat,
    radius: jfloat,
    paint: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        let cfg = paint_config_from_handle(paint);
        match canvas_registry::with_canvas(canvas, |c| c.draw_circle(cx, cy, radius, &cfg)) {
            Ok(()) => tracing::trace!(
                target: "android.graphics.Canvas",
                canvas, cx, cy, radius,
                "Canvas.nDrawCircle: rasterized a circle (real tiny-skia)"
            ),
            Err(e) => tracing::debug!(
                target: "android.graphics.Canvas",
                canvas, error = %e,
                "Canvas.nDrawCircle: invalid canvas handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn canvas_n_draw_path<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    canvas: jlong,
    path: jlong,
    paint: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        let geometry = path_registry::with_path(path, |g| g.clone());
        let Ok(geometry) = geometry else {
            tracing::debug!(
                target: "android.graphics.Canvas",
                canvas, path,
                "Canvas.nDrawPath: invalid path handle (ignored)"
            );
            return Ok(());
        };

        let cfg = paint_config_from_handle(paint);
        match canvas_registry::with_canvas(canvas, |c| c.draw_path(&geometry, &cfg)) {
            Ok(()) => tracing::trace!(
                target: "android.graphics.Canvas",
                canvas, path,
                "Canvas.nDrawPath: rasterized a path (real tiny-skia)"
            ),
            Err(e) => tracing::debug!(
                target: "android.graphics.Canvas",
                canvas, error = %e,
                "Canvas.nDrawPath: invalid canvas handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

fn register_canvas_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(CANVAS_CLASS)?;
    let natives = [
        (
            CANVAS_N_DRAW_COLOR_NAME,
            CANVAS_N_DRAW_COLOR_SIG,
            canvas_n_draw_color as *mut std::ffi::c_void,
        ),
        (
            CANVAS_N_DRAW_RECT_NAME,
            CANVAS_N_DRAW_RECT_SIG,
            canvas_n_draw_rect as *mut std::ffi::c_void,
        ),
        (
            CANVAS_N_DRAW_CIRCLE_NAME,
            CANVAS_N_DRAW_CIRCLE_SIG,
            canvas_n_draw_circle as *mut std::ffi::c_void,
        ),
        (
            CANVAS_N_DRAW_PATH_NAME,
            CANVAS_N_DRAW_PATH_SIG,
            canvas_n_draw_path as *mut std::ffi::c_void,
        ),
    ];
    let declared = natives
        .iter()
        .all(|(name, sig, _)| env.get_static_method_id(&class, name, sig).is_ok());
    if !declared {
        if env.exception_check() {
            env.exception_clear();
        }
        tracing::debug!(
            class = "android/graphics/Canvas",
            "the framework's Canvas declares no nDraw* natives; draw cascade disabled"
        );
        return Ok(());
    }
    let methods = natives.each_ref().map(|(name, sig, function)| unsafe {
        NativeMethod::from_raw_parts(name, sig.sig(), *function)
    });

    match unsafe { env.register_native_methods(&class, &methods) } {
        Ok(()) => {
            CANVAS_DRAW_SUPPORTED.store(true, std::sync::atomic::Ordering::Release);
            tracing::info!(
                class = "android/graphics/Canvas",
                "registered Eclipse's non-GTK backing for Canvas.nDrawColor + nDrawRect + nDrawCircle + nDrawPath (real tiny-skia raster); draw cascade enabled"
            );
        }
        Err(e) => {
            if env.exception_check() {
                env.exception_clear();
            }
            CANVAS_DRAW_SUPPORTED.store(false, std::sync::atomic::Ordering::Release);
            tracing::warn!(
                class = "android/graphics/Canvas",
                error = %e,
                "Canvas draw natives not bindable on this ART build (Canvas is GskCanvas/Bitmap-backed, not nDraw*-native); draw cascade DISABLED — view quads + text still render"
            );
        }
    }
    Ok(())
}

pub fn canvas_draw_supported() -> bool {
    CANVAS_DRAW_SUPPORTED.load(std::sync::atomic::Ordering::Acquire)
}

pub const TEXT_VIEW_CLASS: &JNIStr = jni_str!("android/widget/TextView");

const TEXT_VIEW_NATIVE_SET_TEXT_NAME: &JNIStr = jni_str!("native_setText");
const TEXT_VIEW_NATIVE_SET_TEXT_SIG: &JNIStr = jni_str!("(Ljava/lang/String;)V");

const TEXT_VIEW_NATIVE_SET_TEXT_COLOR_NAME: &JNIStr = jni_str!("native_setTextColor");
const TEXT_VIEW_NATIVE_SET_TEXT_COLOR_SIG: &JNIStr = jni_str!("(I)V");

const TEXT_VIEW_SET_TEXT_SIZE_NAME: &JNIStr = jni_str!("setTextSize");
const TEXT_VIEW_SET_TEXT_SIZE_SIG: &JNIStr = jni_str!("(F)V");

const TEXT_VIEW_NATIVE_SET_MARKUP_NAME: &JNIStr = jni_str!("native_set_markup");
const TEXT_VIEW_NATIVE_SET_MARKUP_SIG: &JNIStr = jni_str!("(I)V");

const TEXT_VIEW_NATIVE_SET_COMPOUND_DRAWABLES_NAME: &JNIStr =
    jni_str!("native_setCompoundDrawables");
const TEXT_VIEW_NATIVE_SET_COMPOUND_DRAWABLES_SIG: &JNIStr = jni_str!("(JJJJJ)V");

const VIEW_WIDGET_FIELD_NAME: &JNIStr = jni_str!("widget");
const VIEW_WIDGET_FIELD_SIG: &JNIStr = jni_str!("J");

const ARRAY_LIST_SIG: &JNIStr = jni_str!("Ljava/util/ArrayList;");

const SURFACE_HOLDER_SIG: &JNIStr = jni_str!("Landroid/view/SurfaceHolder;");

extern "system" fn text_view_native_set_text<'local>(
    mut env: EnvUnowned<'local>,
    this: JObject<'local>,
    text: JString<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let widget = view_widget_handle(env, &this);

        let value = if text.is_null() {
            None
        } else {
            Some(text.try_to_string(env)?)
        };
        match view_registry::with_view(widget, |v| v.text = value.clone()) {
            Ok(()) => tracing::debug!(
                target: "android.widget.TextView",
                widget,
                chars = value.as_deref().map_or(0, |text| text.chars().count()),
                "TextView.native_setText: recorded text length on non-GTK view peer"
            ),
            Err(e) => tracing::debug!(
                target: "android.widget.TextView",
                widget,
                error = %e,
                "TextView.native_setText: invalid view handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn text_view_native_set_text_color<'local>(
    mut env: EnvUnowned<'local>,
    this: JObject<'local>,
    color: jint,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let widget = view_widget_handle(env, &this);
        if let Err(e) = view_registry::with_view(widget, |_v| ()) {
            tracing::debug!(
                target: "android.widget.TextView",
                widget,
                color = format_args!("0x{:08x}", u32::from_ne_bytes(color.to_ne_bytes())),
                error = %e,
                "TextView.native_setTextColor: invalid view handle (ignored)"
            );
        } else {
            tracing::trace!(
                target: "android.widget.TextView",
                widget,
                color = format_args!("0x{:08x}", u32::from_ne_bytes(color.to_ne_bytes())),
                "TextView.native_setTextColor: validated handle, no-op (renderer uses fixed text color)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn text_view_set_text_size<'local>(
    mut env: EnvUnowned<'local>,
    this: JObject<'local>,
    size: jfloat,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let widget = view_widget_handle(env, &this);
        if let Err(e) = view_registry::with_view(widget, |_v| ()) {
            tracing::debug!(
                target: "android.widget.TextView",
                widget,
                size,
                error = %e,
                "TextView.setTextSize: invalid view handle (ignored)"
            );
        } else {
            tracing::trace!(
                target: "android.widget.TextView",
                widget,
                size,
                "TextView.setTextSize: validated handle, no-op (renderer uses fixed glyph metrics)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn text_view_native_set_markup<'local>(
    mut env: EnvUnowned<'local>,
    this: JObject<'local>,
    enable: jint,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let widget = view_widget_handle(env, &this);
        if let Err(e) = view_registry::with_view(widget, |_v| ()) {
            tracing::debug!(
                target: "android.widget.TextView",
                widget,
                enable,
                error = %e,
                "TextView.native_set_markup: invalid view handle (ignored)"
            );
        } else {
            tracing::trace!(
                target: "android.widget.TextView",
                widget,
                enable,
                "TextView.native_set_markup: validated handle, no-op (plain-text peer)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn text_view_native_set_compound_drawables<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    left: jlong,
    top: jlong,
    right: jlong,
    bottom: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = view_registry::with_view(widget, |_v| ()) {
            tracing::debug!(
                target: "android.widget.TextView",
                widget,
                left,
                top,
                right,
                bottom,
                error = %e,
                "TextView.native_setCompoundDrawables: invalid view handle (ignored)"
            );
        } else {
            tracing::trace!(
                target: "android.widget.TextView",
                widget,
                left,
                top,
                right,
                bottom,
                "TextView.native_setCompoundDrawables: validated handle, no-op (drawable draw deferred)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

fn view_widget_handle(env: &mut Env, this: &JObject) -> jlong {
    let sig = unsafe {
        FieldSignature::from_raw_parts(VIEW_WIDGET_FIELD_SIG, JavaType::Primitive(Primitive::Long))
    };
    env.get_field(this, VIEW_WIDGET_FIELD_NAME, &sig)
        .and_then(|v| v.j())
        .unwrap_or(0)
}

fn register_text_view_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let bindings: [NativeBinding; 6] = [
        (
            VIEW_NATIVE_CONSTRUCTOR_NAME,
            VIEW_NATIVE_CONSTRUCTOR_SIG,
            view_native_constructor as *mut c_void,
        ),
        (
            TEXT_VIEW_NATIVE_SET_TEXT_NAME,
            TEXT_VIEW_NATIVE_SET_TEXT_SIG,
            text_view_native_set_text as *mut c_void,
        ),
        (
            TEXT_VIEW_NATIVE_SET_TEXT_COLOR_NAME,
            TEXT_VIEW_NATIVE_SET_TEXT_COLOR_SIG,
            text_view_native_set_text_color as *mut c_void,
        ),
        (
            TEXT_VIEW_SET_TEXT_SIZE_NAME,
            TEXT_VIEW_SET_TEXT_SIZE_SIG,
            text_view_set_text_size as *mut c_void,
        ),
        (
            TEXT_VIEW_NATIVE_SET_MARKUP_NAME,
            TEXT_VIEW_NATIVE_SET_MARKUP_SIG,
            text_view_native_set_markup as *mut c_void,
        ),
        (
            TEXT_VIEW_NATIVE_SET_COMPOUND_DRAWABLES_NAME,
            TEXT_VIEW_NATIVE_SET_COMPOUND_DRAWABLES_SIG,
            text_view_native_set_compound_drawables as *mut c_void,
        ),
    ];
    let bound = register_class_natives_best_effort(env, TEXT_VIEW_CLASS, &bindings)?;
    tracing::info!(
        class = "android/widget/TextView",
        bound,
        "registered Eclipse's non-GTK backing for TextView.native_constructor + native_setText + native_setTextColor + setTextSize + native_set_markup + native_setCompoundDrawables (per-method best-effort)"
    );
    Ok(())
}

pub const IMAGE_VIEW_CLASS: &JNIStr = jni_str!("android/widget/ImageView");

const IMAGE_VIEW_SET_SCALE_TYPE_NAME: &JNIStr = jni_str!("native_setScaleType");
const IMAGE_VIEW_SET_SCALE_TYPE_SIG: &JNIStr = jni_str!("(JI)V");

const IMAGE_VIEW_SET_DRAWABLE_NAME: &JNIStr = jni_str!("native_setDrawable");
const IMAGE_VIEW_SET_DRAWABLE_SIG: &JNIStr = jni_str!("(JJ)V");

extern "system" fn image_view_set_scale_type<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    scale_type: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        match view_registry::with_view(widget, |_v| ()) {
            Ok(()) => tracing::trace!(
                target: "android.widget.ImageView",
                widget,
                scale_type,
                "ImageView.native_setScaleType: validated handle (no-op; no image draw consumer yet)"
            ),
            Err(e) => tracing::debug!(
                target: "android.widget.ImageView",
                widget,
                error = %e,
                "ImageView.native_setScaleType: invalid view handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn image_view_set_drawable<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    drawable: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        match view_registry::with_view(widget, |_v| ()) {
            Ok(()) => tracing::trace!(
                target: "android.widget.ImageView",
                widget,
                drawable,
                "ImageView.native_setDrawable: validated handle (no-op; no image draw consumer yet)"
            ),
            Err(e) => tracing::debug!(
                target: "android.widget.ImageView",
                widget,
                error = %e,
                "ImageView.native_setDrawable: invalid view handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

fn register_image_view_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let bindings: [NativeBinding; 3] = [
        (
            VIEW_NATIVE_CONSTRUCTOR_NAME,
            VIEW_NATIVE_CONSTRUCTOR_SIG,
            view_native_constructor as *mut c_void,
        ),
        (
            IMAGE_VIEW_SET_SCALE_TYPE_NAME,
            IMAGE_VIEW_SET_SCALE_TYPE_SIG,
            image_view_set_scale_type as *mut c_void,
        ),
        (
            IMAGE_VIEW_SET_DRAWABLE_NAME,
            IMAGE_VIEW_SET_DRAWABLE_SIG,
            image_view_set_drawable as *mut c_void,
        ),
    ];
    let bound = register_class_natives_best_effort(env, IMAGE_VIEW_CLASS, &bindings)?;
    tracing::info!(
        class = "android/widget/ImageView",
        bound,
        "registered Eclipse's non-GTK backing for ImageView.native_constructor + native_setScaleType + native_setDrawable (per-method best-effort)"
    );
    Ok(())
}

pub const IMAGE_BUTTON_CLASS: &JNIStr = jni_str!("android/widget/ImageButton");

const IMAGE_BUTTON_SET_ON_CLICK_LISTENER_NAME: &JNIStr = jni_str!("nativeSetOnClickListener");
const IMAGE_BUTTON_SET_ON_CLICK_LISTENER_SIG: &JNIStr = jni_str!("(J)V");

extern "system" fn image_button_set_on_click_listener<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        match view_registry::set_clickable(widget) {
            Ok(()) => tracing::debug!(
                target: "android.widget.ImageButton",
                widget,
                "View.nativeSetOnClickListener: marked view clickable (hit-test will target it)"
            ),
            Err(e) => tracing::debug!(
                target: "android.widget.ImageButton",
                widget,
                error = %e,
                "ImageButton.nativeSetOnClickListener: invalid view handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn view_set_input_listener<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        match view_registry::with_view(widget, |_v| ()) {
            Ok(()) => tracing::debug!(
                target: "android.view.View",
                widget,
                "View.nativeSetOnTouch/LongClickListener: listener kept Java-side (engine input path dispatches)"
            ),
            Err(e) => tracing::debug!(
                target: "android.view.View",
                widget,
                error = %e,
                "View.nativeSetOnTouch/LongClickListener: invalid view handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

fn register_image_button_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let bindings: [NativeBinding; 3] = [
        (
            VIEW_NATIVE_CONSTRUCTOR_NAME,
            VIEW_NATIVE_CONSTRUCTOR_SIG,
            view_native_constructor as *mut c_void,
        ),
        (
            IMAGE_BUTTON_SET_ON_CLICK_LISTENER_NAME,
            IMAGE_BUTTON_SET_ON_CLICK_LISTENER_SIG,
            image_button_set_on_click_listener as *mut c_void,
        ),
        (
            IMAGE_VIEW_SET_DRAWABLE_NAME,
            IMAGE_VIEW_SET_DRAWABLE_SIG,
            image_view_set_drawable as *mut c_void,
        ),
    ];
    let bound = register_class_natives_best_effort(env, IMAGE_BUTTON_CLASS, &bindings)?;
    tracing::info!(
        class = "android/widget/ImageButton",
        bound,
        "registered Eclipse's non-GTK backing for ImageButton.native_constructor + nativeSetOnClickListener + native_setDrawable (per-method best-effort)"
    );
    Ok(())
}

pub const SURFACE_VIEW_CLASS: &JNIStr = jni_str!("android/view/SurfaceView");

fn register_surface_view_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let bindings: [NativeBinding; 1] = [(
        VIEW_NATIVE_CONSTRUCTOR_NAME,
        VIEW_NATIVE_CONSTRUCTOR_SIG,
        view_native_constructor as *mut c_void,
    )];
    let bound = register_class_natives_best_effort(env, SURFACE_VIEW_CLASS, &bindings)?;
    tracing::info!(
        class = "android/view/SurfaceView",
        bound,
        "registered Eclipse's non-GTK backing for SurfaceView.native_constructor (per-method best-effort)"
    );
    Ok(())
}

pub const BUTTON_CLASS: &JNIStr = jni_str!("android/widget/Button");
pub const EDIT_TEXT_CLASS: &JNIStr = jni_str!("android/widget/EditText");
pub const PROGRESS_BAR_CLASS: &JNIStr = jni_str!("android/widget/ProgressBar");
pub const CHECK_BOX_CLASS: &JNIStr = jni_str!("android/widget/CheckBox");
pub const RADIO_BUTTON_CLASS: &JNIStr = jni_str!("android/widget/RadioButton");
pub const SEEK_BAR_CLASS: &JNIStr = jni_str!("android/widget/SeekBar");
pub const SPINNER_CLASS: &JNIStr = jni_str!("android/widget/Spinner");
pub const SCROLL_VIEW_CLASS: &JNIStr = jni_str!("android/widget/ScrollView");

const VIEW_SUBCLASS_CONSTRUCTOR_CLASSES: &[&JNIStr] = &[
    BUTTON_CLASS,
    EDIT_TEXT_CLASS,
    PROGRESS_BAR_CLASS,
    CHECK_BOX_CLASS,
    RADIO_BUTTON_CLASS,
    SEEK_BAR_CLASS,
    SPINNER_CLASS,
    SCROLL_VIEW_CLASS,
];

fn register_view_subclass_constructor_natives(env: &mut Env) -> Result<(), FrameworkError> {
    for &class_name in VIEW_SUBCLASS_CONSTRUCTOR_CLASSES {
        let bindings: [NativeBinding; 1] = [(
            VIEW_NATIVE_CONSTRUCTOR_NAME,
            VIEW_NATIVE_CONSTRUCTOR_SIG,
            view_native_constructor as *mut c_void,
        )];
        let bound = register_class_natives_best_effort(env, class_name, &bindings)?;
        tracing::info!(
            class = %class_name.to_str(),
            bound,
            "registered Eclipse's non-GTK backing for native_constructor on inflatable View subclass (per-method best-effort)"
        );
    }
    Ok(())
}

pub const WEB_VIEW_CLASS: &JNIStr = jni_str!("android/webkit/WebView");

const WEB_VIEW_NATIVE_LOAD_URL_NAME: &JNIStr = jni_str!("native_loadUrl");
const WEB_VIEW_NATIVE_LOAD_URL_SIG: &JNIStr = jni_str!("(JLjava/lang/String;)V");
const WEB_VIEW_NATIVE_LOAD_DATA_WITH_BASE_URL_NAME: &JNIStr =
    jni_str!("native_loadDataWithBaseURL");
const WEB_VIEW_NATIVE_LOAD_DATA_WITH_BASE_URL_SIG: &JNIStr =
    jni_str!("(JLjava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V");
const WEB_VIEW_NATIVE_CAN_GO_BACK_NAME: &JNIStr = jni_str!("native_canGoBack");
const WEB_VIEW_NATIVE_CAN_GO_BACK_SIG: &JNIStr = jni_str!("(J)Z");
const WEB_VIEW_NATIVE_GO_BACK_NAME: &JNIStr = jni_str!("native_goBack");
const WEB_VIEW_NATIVE_GO_BACK_SIG: &JNIStr = jni_str!("(J)V");
const WEB_VIEW_NATIVE_RELOAD_NAME: &JNIStr = jni_str!("native_reload");
const WEB_VIEW_NATIVE_RELOAD_SIG: &JNIStr = jni_str!("(J)V");
const WEB_VIEW_NATIVE_STOP_LOADING_NAME: &JNIStr = jni_str!("native_stopLoading");
const WEB_VIEW_NATIVE_STOP_LOADING_SIG: &JNIStr = jni_str!("(J)V");
const WEB_VIEW_NATIVE_GET_URL_NAME: &JNIStr = jni_str!("native_getUrl");
const WEB_VIEW_NATIVE_GET_URL_SIG: &JNIStr = jni_str!("(J)Ljava/lang/String;");
const WEB_VIEW_NATIVE_REMOVE_JAVASCRIPT_INTERFACE_NAME: &JNIStr =
    jni_str!("native_removeJavascriptInterface");
const WEB_VIEW_NATIVE_REMOVE_JAVASCRIPT_INTERFACE_SIG: &JNIStr = jni_str!("(JLjava/lang/String;)V");
const WEB_VIEW_NATIVE_DESTROY_NAME: &JNIStr = jni_str!("native_destroy");
const WEB_VIEW_NATIVE_DESTROY_SIG: &JNIStr = jni_str!("(J)V");

use crate::webview::redact::{redact_urls_for_log, url_scheme_and_host_for_log, NON_URL};

fn warn_load_url_unavailable(widget: jlong, target: &str, reason: &str) {
    static LOAD_URL_WARNED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    if !LOAD_URL_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!(
            target: "android.webkit.WebView",
            widget,
            %target,
            reason,
            "WebView.native_loadUrl: web engine helper unavailable — content will not load \
             (honest no-op; target redacted to scheme+host)"
        );
    } else {
        tracing::debug!(
            target: "android.webkit.WebView",
            widget,
            %target,
            reason,
            "WebView.native_loadUrl: web engine helper unavailable (honest no-op)"
        );
    }
}

fn warn_load_data_unavailable(widget: jlong, base: &str, reason: &str) {
    static LOAD_DATA_WARNED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    if !LOAD_DATA_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!(
            target: "android.webkit.WebView",
            widget,
            %base,
            reason,
            "WebView.native_loadDataWithBaseURL: web engine helper unavailable — content will \
             not load (honest no-op; baseUrl redacted, data payload never logged)"
        );
    } else {
        tracing::debug!(
            target: "android.webkit.WebView",
            widget,
            %base,
            reason,
            "WebView.native_loadDataWithBaseURL: web engine helper unavailable (honest no-op)"
        );
    }
}

fn report_web_view_op(
    op: &'static str,
    widget: jlong,
    result: Result<(), crate::webview::client::ClientError>,
) {
    if let Err(error) = result {
        tracing::warn!(
            target: "android.webkit.WebView",
            widget,
            op,
            %error,
            "WebView operation not delivered to the web engine helper"
        );
    }
}

extern "system" fn web_view_native_load_url<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    url: JString<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if let Err(e) = view_registry::with_view(widget, |_v| ()) {
            tracing::warn!(
                target: "android.webkit.WebView",
                widget,
                error = %e,
                "WebView.native_loadUrl: invalid view handle (ignored)"
            );
            return Ok(());
        }
        let Some(full) = read_jstring(env, &url) else {
            warn_load_url_unavailable(widget, NON_URL, "load target null/unreadable");
            return Ok(());
        };
        let target = url_scheme_and_host_for_log(&full);
        match env.get_java_vm() {
            Ok(java_vm) => match crate::webview::client::load_url(java_vm, widget, full) {
                Ok(()) => tracing::info!(
                    target: "android.webkit.WebView",
                    widget,
                    %target,
                    "WebView.native_loadUrl: forwarded to the eclipse-webview helper \
                     (target redacted to scheme+host)"
                ),
                Err(e) => warn_load_url_unavailable(widget, &target, &e.to_string()),
            },
            Err(e) => {
                warn_load_url_unavailable(widget, &target, &format!("JavaVM unavailable: {e}"));
            }
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn web_view_native_load_data_with_base_url<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    base_url: JString<'local>,
    data: JString<'local>,
    mime: JString<'local>,
    encoding: JString<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if let Err(e) = view_registry::with_view(widget, |_v| ()) {
            tracing::warn!(
                target: "android.webkit.WebView",
                widget,
                error = %e,
                "WebView.native_loadDataWithBaseURL: invalid view handle (ignored)"
            );
            return Ok(());
        }
        let base = read_jstring(env, &base_url);
        let data_s = read_jstring(env, &data);
        let mime_s = read_jstring(env, &mime);
        let encoding_s = read_jstring(env, &encoding);

        let log_base = base
            .as_deref()
            .map(url_scheme_and_host_for_log)
            .unwrap_or_else(|| NON_URL.to_string());
        let log_mime = mime_s.clone().unwrap_or_else(|| String::from("<none>"));
        let Some(data_s) = data_s else {
            warn_load_data_unavailable(widget, &log_base, "data payload null/unreadable");
            return Ok(());
        };
        match env.get_java_vm() {
            Ok(java_vm) => match crate::webview::client::load_data(
                java_vm,
                widget,
                crate::webview::client::DataLoad {
                    base_url: base,
                    data: data_s,
                    mime: mime_s,
                    encoding: encoding_s,
                },
            ) {
                Ok(()) => tracing::info!(
                    target: "android.webkit.WebView",
                    widget,
                    base = %log_base,
                    mime = %log_mime,
                    "WebView.native_loadDataWithBaseURL: forwarded to the eclipse-webview \
                     helper (baseUrl redacted, data payload never logged)"
                ),
                Err(e) => warn_load_data_unavailable(widget, &log_base, &e.to_string()),
            },
            Err(e) => {
                warn_load_data_unavailable(widget, &log_base, &format!("JavaVM unavailable: {e}"));
            }
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn web_view_native_can_go_back<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
) -> jboolean {
    env.with_env(|_env| -> jni::errors::Result<jboolean> {
        Ok(crate::webview::client::can_go_back(widget))
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn web_view_native_go_back<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        report_web_view_op("goBack", widget, crate::webview::client::go_back(widget));
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn web_view_native_reload<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        report_web_view_op("reload", widget, crate::webview::client::reload(widget));
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn web_view_native_stop_loading<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        report_web_view_op(
            "stopLoading",
            widget,
            crate::webview::client::stop_loading(widget),
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn web_view_native_get_url<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        match crate::webview::client::url(widget) {
            Some(url) => env.new_string(url),
            None => Ok(JString::default()),
        }
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn web_view_native_remove_javascript_interface<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    name: JString<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let Some(name) = read_jstring(env, &name) else {
            return Ok(());
        };
        drop_bridge(widget, &name);
        report_web_view_op(
            "removeJavascriptInterface",
            widget,
            crate::webview::client::unregister_bridge(widget, name),
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn web_view_native_destroy<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if has_webview_bridges() {
            drop_bridges_for(widget);
        }
        report_web_view_op(
            "destroy",
            widget,
            crate::webview::client::close_view(widget),
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

fn register_web_view_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let bindings: [NativeBinding; 12] = [
        (
            VIEW_NATIVE_CONSTRUCTOR_NAME,
            VIEW_NATIVE_CONSTRUCTOR_SIG,
            view_native_constructor as *mut c_void,
        ),
        (
            WEB_VIEW_NATIVE_LOAD_URL_NAME,
            WEB_VIEW_NATIVE_LOAD_URL_SIG,
            web_view_native_load_url as *mut c_void,
        ),
        (
            WEB_VIEW_NATIVE_LOAD_DATA_WITH_BASE_URL_NAME,
            WEB_VIEW_NATIVE_LOAD_DATA_WITH_BASE_URL_SIG,
            web_view_native_load_data_with_base_url as *mut c_void,
        ),
        (
            WEB_VIEW_NATIVE_EVALUATE_JAVASCRIPT_NAME,
            WEB_VIEW_NATIVE_EVALUATE_JAVASCRIPT_SIG,
            web_view_native_evaluate_javascript as *mut c_void,
        ),
        (
            WEB_VIEW_NATIVE_ADD_JAVASCRIPT_INTERFACE_NAME,
            WEB_VIEW_NATIVE_ADD_JAVASCRIPT_INTERFACE_SIG,
            web_view_native_add_javascript_interface as *mut c_void,
        ),
        (
            WEB_VIEW_NATIVE_REMOVE_JAVASCRIPT_INTERFACE_NAME,
            WEB_VIEW_NATIVE_REMOVE_JAVASCRIPT_INTERFACE_SIG,
            web_view_native_remove_javascript_interface as *mut c_void,
        ),
        (
            WEB_VIEW_NATIVE_CAN_GO_BACK_NAME,
            WEB_VIEW_NATIVE_CAN_GO_BACK_SIG,
            web_view_native_can_go_back as *mut c_void,
        ),
        (
            WEB_VIEW_NATIVE_GO_BACK_NAME,
            WEB_VIEW_NATIVE_GO_BACK_SIG,
            web_view_native_go_back as *mut c_void,
        ),
        (
            WEB_VIEW_NATIVE_RELOAD_NAME,
            WEB_VIEW_NATIVE_RELOAD_SIG,
            web_view_native_reload as *mut c_void,
        ),
        (
            WEB_VIEW_NATIVE_STOP_LOADING_NAME,
            WEB_VIEW_NATIVE_STOP_LOADING_SIG,
            web_view_native_stop_loading as *mut c_void,
        ),
        (
            WEB_VIEW_NATIVE_GET_URL_NAME,
            WEB_VIEW_NATIVE_GET_URL_SIG,
            web_view_native_get_url as *mut c_void,
        ),
        (
            WEB_VIEW_NATIVE_DESTROY_NAME,
            WEB_VIEW_NATIVE_DESTROY_SIG,
            web_view_native_destroy as *mut c_void,
        ),
    ];
    let bound = register_class_natives_best_effort(env, WEB_VIEW_CLASS, &bindings)?;
    tracing::info!(
        bound,
        "registered Eclipse's backing for the android.webkit.WebView native surface (constructor = shared view-registry peer; loads, history, reload, stopLoading, getUrl, destroy, evaluateJavascript and the JavaScript bridge forward to the WebKitGTK helper, which shows each WebView inside the game window on Wayland and as a dialog of it elsewhere) (per-method best-effort)"
    );
    Ok(())
}

pub const ACTIVITY_MANAGER_CLASS: &JNIStr = jni_str!("android/app/ActivityManager");

const AM_NATIVE_FILL_MEMORY_INFO_NAME: &JNIStr = jni_str!("native_fillMemoryInfo");
const AM_NATIVE_FILL_MEMORY_INFO_SIG: &JNIStr =
    jni_str!("(Landroid/app/ActivityManager$MemoryInfo;)V");
const AM_NATIVE_GET_MEMORY_CLASS_NAME: &JNIStr = jni_str!("native_getMemoryClass");
const AM_NATIVE_GET_MEMORY_CLASS_SIG: &JNIStr = jni_str!("()I");
const AM_NATIVE_GET_LARGE_MEMORY_CLASS_NAME: &JNIStr = jni_str!("native_getLargeMemoryClass");
const AM_NATIVE_GET_LARGE_MEMORY_CLASS_SIG: &JNIStr = jni_str!("()I");
const AM_NATIVE_IS_LOW_RAM_DEVICE_NAME: &JNIStr = jni_str!("native_isLowRamDevice");
const AM_NATIVE_IS_LOW_RAM_DEVICE_SIG: &JNIStr = jni_str!("()Z");

const MEMORY_INFO_AVAIL_MEM_FIELD: &JNIStr = jni_str!("availMem");
const MEMORY_INFO_TOTAL_MEM_FIELD: &JNIStr = jni_str!("totalMem");
const MEMORY_INFO_THRESHOLD_FIELD: &JNIStr = jni_str!("threshold");
const MEMORY_INFO_LOW_MEMORY_FIELD: &JNIStr = jni_str!("lowMemory");
const MEMORY_INFO_HIDDEN_THRESHOLD_FIELD: &JNIStr = jni_str!("hiddenAppThreshold");
const MEMORY_INFO_SECONDARY_THRESHOLD_FIELD: &JNIStr = jni_str!("secondaryServerThreshold");
const MEMORY_INFO_VISIBLE_THRESHOLD_FIELD: &JNIStr = jni_str!("visibleAppThreshold");
const MEMORY_INFO_FOREGROUND_THRESHOLD_FIELD: &JNIStr = jni_str!("foregroundAppThreshold");
const LONG_FIELD_SIG: &JNIStr = jni_str!("J");

fn memory_bytes_to_jlong(bytes: u64) -> jlong {
    jlong::try_from(bytes).unwrap_or(jlong::MAX)
}

extern "system" fn activity_manager_native_fill_memory_info<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    out_info: JObject<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let Some(snapshot) = memory::host_memory_snapshot() else {
            tracing::warn!(
                target: "android.app.ActivityManager",
                "getMemoryInfo: neither /proc/meminfo nor sysinfo produced a truthful memory \
                 snapshot; leaving the caller's zero-initialized MemoryInfo unchanged"
            );
            return Ok(());
        };

        let long_sig = unsafe {
            FieldSignature::from_raw_parts(LONG_FIELD_SIG, JavaType::Primitive(Primitive::Long))
        };
        let values = [
            (
                MEMORY_INFO_AVAIL_MEM_FIELD,
                memory_bytes_to_jlong(snapshot.available_bytes),
            ),
            (
                MEMORY_INFO_TOTAL_MEM_FIELD,
                memory_bytes_to_jlong(snapshot.total_bytes),
            ),
            (MEMORY_INFO_THRESHOLD_FIELD, 0),
            (MEMORY_INFO_HIDDEN_THRESHOLD_FIELD, 0),
            (MEMORY_INFO_SECONDARY_THRESHOLD_FIELD, 0),
            (MEMORY_INFO_VISIBLE_THRESHOLD_FIELD, 0),
            (MEMORY_INFO_FOREGROUND_THRESHOLD_FIELD, 0),
        ];
        for (field, value) in values {
            env.set_field(&out_info, field, &long_sig, JValue::Long(value))?;
        }

        let boolean_sig = unsafe {
            FieldSignature::from_raw_parts(
                BOOLEAN_FIELD_SIG,
                JavaType::Primitive(Primitive::Boolean),
            )
        };
        env.set_field(
            &out_info,
            MEMORY_INFO_LOW_MEMORY_FIELD,
            &boolean_sig,
            JValue::Bool(false),
        )?;

        tracing::info!(
            target: "android.app.ActivityManager",
            total_mib = snapshot.total_mib(),
            available_mib = snapshot.available_bytes / (1024 * 1024),
            "getMemoryInfo: populated the caller's object from the Linux kernel"
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn activity_manager_native_get_memory_class<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jint {
    env.with_env(|_env| -> jni::errors::Result<jint> {
        Ok(jint::try_from(memory::managed_heap_mib()).unwrap_or(jint::MAX))
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn activity_manager_native_get_large_memory_class<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jint {
    env.with_env(|_env| -> jni::errors::Result<jint> {
        Ok(jint::try_from(memory::managed_heap_mib()).unwrap_or(jint::MAX))
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn activity_manager_native_is_low_ram_device<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jboolean {
    env.with_env(|_env| -> jni::errors::Result<jboolean> {
        let Some(snapshot) = memory::host_memory_snapshot() else {
            tracing::warn!(
                target: "android.app.ActivityManager",
                "isLowRamDevice: host memory could not be detected; returning false"
            );
            return Ok(false);
        };
        Ok(snapshot.is_low_ram_device())
    })
    .resolve::<LogErrorAndDefault>()
}

fn register_activity_manager_memory_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let bindings: [NativeBinding; 4] = [
        (
            AM_NATIVE_FILL_MEMORY_INFO_NAME,
            AM_NATIVE_FILL_MEMORY_INFO_SIG,
            activity_manager_native_fill_memory_info as *mut c_void,
        ),
        (
            AM_NATIVE_GET_MEMORY_CLASS_NAME,
            AM_NATIVE_GET_MEMORY_CLASS_SIG,
            activity_manager_native_get_memory_class as *mut c_void,
        ),
        (
            AM_NATIVE_GET_LARGE_MEMORY_CLASS_NAME,
            AM_NATIVE_GET_LARGE_MEMORY_CLASS_SIG,
            activity_manager_native_get_large_memory_class as *mut c_void,
        ),
        (
            AM_NATIVE_IS_LOW_RAM_DEVICE_NAME,
            AM_NATIVE_IS_LOW_RAM_DEVICE_SIG,
            activity_manager_native_is_low_ram_device as *mut c_void,
        ),
    ];
    let bound = register_class_natives_best_effort(env, ACTIVITY_MANAGER_CLASS, &bindings)?;
    tracing::info!(
        bound,
        "registered Linux-backed ActivityManager memory APIs (kernel total/available RAM, actual \
         ART heap class, detected low-RAM class; Android LMK thresholds honestly unavailable) \
         (per-method best-effort)"
    );
    Ok(())
}

pub const WEB_SETTINGS_CLASS: &JNIStr = jni_str!("android/webkit/WebSettings");

const WEB_SETTINGS_NATIVE_SET_USER_AGENT_STRING_NAME: &JNIStr =
    jni_str!("native_setUserAgentString");
const WEB_SETTINGS_NATIVE_SET_USER_AGENT_STRING_SIG: &JNIStr = jni_str!("(JLjava/lang/String;)V");

extern "system" fn web_settings_native_set_user_agent_string<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    ua: JString<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let user_agent = if ua.is_null() {
            None
        } else {
            let Some(ua) = read_jstring(env, &ua) else {
                tracing::warn!(
                    target: "android.webkit.WebSettings",
                    widget,
                    "setUserAgentString: the UA string was unreadable (exception cleared) — the \
                     previously set User-Agent stands"
                );
                return Ok(());
            };
            Some(ua)
        };
        tracing::info!(
            target: "android.webkit.WebSettings",
            widget,
            ua = user_agent.as_deref().unwrap_or("<reset to default>"),
            "the app set its WebView User-Agent; Eclipse hands it to the engine unchanged \
             (AOSP contract: null/empty resets to the default)"
        );
        report_web_view_op(
            "setUserAgentString",
            widget,
            crate::webview::client::set_user_agent(widget, user_agent),
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

fn register_web_settings_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let bindings: [NativeBinding; 1] = [(
        WEB_SETTINGS_NATIVE_SET_USER_AGENT_STRING_NAME,
        WEB_SETTINGS_NATIVE_SET_USER_AGENT_STRING_SIG,
        web_settings_native_set_user_agent_string as *mut c_void,
    )];
    let bound = register_class_natives_best_effort(env, WEB_SETTINGS_CLASS, &bindings)?;
    tracing::info!(
        bound,
        "registered Eclipse's backing for WebSettings.setUserAgentString (each WebView's \
         User-Agent reaches its own engine view unchanged) (per-method best-effort)"
    );
    Ok(())
}

const WEB_VIEW_INTERNAL_LOAD_CHANGED_NAME: &JNIStr = jni_str!("internalLoadChanged");
const WEB_VIEW_INTERNAL_LOAD_CHANGED_SIG: MethodSignature<'static, 'static> =
    jni_sig!("(ILjava/lang/String;)V");

type MainJob = Box<dyn for<'l> FnOnce(&mut Env<'l>) + Send + 'static>;

struct MainDispatchSlot {
    job: Option<MainJob>,

    posted: std::collections::VecDeque<MainJob>,

    open: bool,
}

static MAIN_DISPATCH: std::sync::Mutex<MainDispatchSlot> =
    std::sync::Mutex::new(MainDispatchSlot {
        job: None,
        posted: std::collections::VecDeque::new(),
        open: true,
    });

static MAIN_THREAD_ID: OnceLock<std::thread::ThreadId> = OnceLock::new();

static MAIN_DISPATCH_DEGRADED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

const MAIN_DISPATCH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MainDispatchGate {
    Post,

    InlineOnMainThread,

    InlineNoMainLooper,

    InlineDrainRetired,

    InlineSlotBusy,
}

fn main_dispatch_gate(
    on_main_thread: bool,
    main_looper_prepared: bool,
    drain_open: bool,
    slot_free: bool,
) -> MainDispatchGate {
    if on_main_thread {
        return MainDispatchGate::InlineOnMainThread;
    }
    if !main_looper_prepared {
        return MainDispatchGate::InlineNoMainLooper;
    }
    if !drain_open {
        return MainDispatchGate::InlineDrainRetired;
    }
    if !slot_free {
        return MainDispatchGate::InlineSlotBusy;
    }
    MainDispatchGate::Post
}

fn dispatch_webview_callback_on_main<R: Send + 'static>(
    java_vm: &JavaVM,
    what: &'static str,
    job: impl for<'l> FnOnce(&mut Env<'l>) -> R + Send + 'static,
) -> Option<R> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<R>(1);
    let mut boxed: Option<MainJob> = Some(Box::new(move |env| {
        let _ = tx.send(job(env));
    }));

    let current = std::thread::current().id();
    let on_main = MAIN_THREAD_ID.get() == Some(&current);
    let prepared = MAIN_THREAD_ID.get().is_some();

    let gate = match MAIN_DISPATCH.lock() {
        Ok(mut slot) => {
            let g = main_dispatch_gate(on_main, prepared, slot.open, slot.job.is_none());
            if g == MainDispatchGate::Post {
                slot.job = boxed.take();
            }
            g
        }

        Err(_) => MainDispatchGate::InlineDrainRetired,
    };

    if let Some(job) = boxed {
        if gate != MainDispatchGate::InlineOnMainThread {
            warn_main_dispatch_degraded(what, gate);
        }
        run_main_job_here(java_vm, job);
        return rx.recv().ok();
    }

    wake_main_looper();
    match rx.recv_timeout(MAIN_DISPATCH_DEADLINE) {
        Ok(r) => Some(r),
        Err(_) => match MAIN_DISPATCH.lock().ok().and_then(|mut s| s.job.take()) {
            Some(job) => {
                warn_main_dispatch_degraded(what, gate);
                run_main_job_here(java_vm, job);
                rx.recv().ok()
            }

            None => rx.recv().ok(),
        },
    }
}

fn post_webview_callback_to_main(
    env: &mut Env,
    what: &'static str,
    job: impl for<'l> FnOnce(&mut Env<'l>) + Send + 'static,
) {
    let gate = if MAIN_THREAD_ID.get().is_none() {
        MainDispatchGate::InlineNoMainLooper
    } else {
        match MAIN_DISPATCH.lock() {
            Ok(mut slot) if slot.open => {
                slot.posted.push_back(Box::new(job));
                drop(slot);
                wake_main_looper();
                return;
            }
            Ok(_) | Err(_) => MainDispatchGate::InlineDrainRetired,
        }
    };
    warn_main_dispatch_degraded(what, gate);
    job(env);
}

fn run_main_job_here(java_vm: &JavaVM, job: MainJob) {
    let _ = java_vm.attach_current_thread(|env: &mut Env| -> Result<(), FrameworkError> {
        match std::panic::catch_unwind(AssertUnwindSafe(|| job(env))) {
            Ok(()) => Ok(()),
            Err(_) => Err(FrameworkError::Panicked),
        }
    });
}

fn warn_main_dispatch_degraded(what: &'static str, gate: MainDispatchGate) {
    if !MAIN_DISPATCH_DEGRADED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!(
            target: "android.webkit.WebView",
            callback = what,
            ?gate,
            "app-facing WebView callback could NOT be delivered on the main/UI thread — running it \
             on this Looper-less thread instead (the pre-2026-07-16 delivery: the app's own \
             new Handler() will throw). Never dropped; logged once. Outside process teardown this \
             means the main Looper pump is not running."
        );
    }
}

fn take_main_jobs(slot: &mut MainDispatchSlot) -> Vec<MainJob> {
    slot.job
        .take()
        .into_iter()
        .chain(std::mem::take(&mut slot.posted))
        .collect()
}

fn run_pending_main_upcall(env: &mut Env) {
    let jobs: Vec<MainJob> = match MAIN_DISPATCH.lock() {
        Ok(mut slot) => take_main_jobs(&mut slot),
        Err(_) => Vec::new(),
    };
    for job in jobs {
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| job(env)));
    }
}

pub fn retire_main_upcall_dispatch(vm: &Vm) {
    let jobs: Vec<MainJob> = match MAIN_DISPATCH.lock() {
        Ok(mut slot) => {
            slot.open = false;
            take_main_jobs(&mut slot)
        }
        Err(_) => return,
    };
    if jobs.is_empty() {
        return;
    }
    let raw = vm.as_raw();
    if raw.is_null() {
        return;
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    for job in jobs {
        run_main_job_here(&java_vm, job);
    }
}

const WEB_VIEW_INTERNAL_PROGRESS_CHANGED_NAME: &JNIStr = jni_str!("internalProgressChanged");
const WEB_VIEW_INTERNAL_PROGRESS_CHANGED_SIG: MethodSignature<'static, 'static> = jni_sig!("(I)V");
const WEB_VIEW_INTERNAL_LOAD_RESOURCE_NAME: &JNIStr = jni_str!("internalLoadResource");
const WEB_VIEW_INTERNAL_LOAD_RESOURCE_SIG: MethodSignature<'static, 'static> =
    jni_sig!("(Ljava/lang/String;)V");
const WEB_VIEW_INTERNAL_RECEIVED_ERROR_NAME: &JNIStr = jni_str!("internalReceivedError");
const WEB_VIEW_INTERNAL_RECEIVED_ERROR_SIG: MethodSignature<'static, 'static> =
    jni_sig!("(Landroid/webkit/WebResourceRequest;Landroid/webkit/WebResourceError;)V");
const WEB_VIEW_INTERNAL_SHOULD_OVERRIDE_URL_LOADING_NAME: &JNIStr =
    jni_str!("internalShouldOverrideUrlLoading");
const WEB_VIEW_INTERNAL_SHOULD_OVERRIDE_URL_LOADING_SIG: MethodSignature<'static, 'static> =
    jni_sig!("(Landroid/webkit/WebResourceRequest;)Z");
const WEB_RESOURCE_REQUEST_CLASS: &JNIStr = jni_str!("android/webkit/EclipseWebResourceRequest");
const WEB_RESOURCE_REQUEST_CTOR_SIG: MethodSignature<'static, 'static> =
    jni_sig!("(Ljava/lang/String;Ljava/lang/String;ZZ)V");
const WEB_RESOURCE_ERROR_CLASS: &JNIStr = jni_str!("android/webkit/WebResourceError");
const WEB_RESOURCE_ERROR_CTOR_SIG: MethodSignature<'static, 'static> =
    jni_sig!("(ILjava/lang/CharSequence;)V");

fn web_view_object<'local>(env: &mut Env<'local>, widget: jlong) -> Option<JObject<'local>> {
    match view_registry::local_jobject(env, widget) {
        Ok(Ok(Some(object))) => Some(object),
        Ok(Err(_)) => {
            clear_pending(env);
            None
        }
        Ok(Ok(None)) | Err(_) => None,
    }
}

fn fire_web_view_callback<R: Send + 'static>(
    java_vm: &JavaVM,
    widget: jlong,
    callback: &'static str,
    call: impl for<'l> FnOnce(&mut Env<'l>, &JObject<'l>) -> Result<R, FrameworkError> + Send + 'static,
) -> Option<R> {
    let delivered = dispatch_webview_callback_on_main(java_vm, callback, move |env: &mut Env| {
        let Some(web_view) = web_view_object(env, widget) else {
            tracing::debug!(
                target: "android.webkit.WebView",
                widget,
                callback,
                "no live WebView object for the callback (not delivered)"
            );
            return None;
        };
        match call(env, &web_view) {
            Ok(result) => Some(result),
            Err(error) => {
                tracing::warn!(
                    target: "android.webkit.WebView",
                    widget,
                    callback,
                    %error,
                    "WebView callback failed (URLs never logged)"
                );
                None
            }
        }
    });
    delivered.flatten()
}

pub fn fire_web_view_internal_load_changed(
    java_vm: &JavaVM,
    widget: jlong,
    state: i32,
    url: &str,
) -> bool {
    let url = url.to_string();
    fire_web_view_callback(
        java_vm,
        widget,
        "WebView.internalLoadChanged",
        move |env, web_view| {
            let url = env.new_string(&url)?;
            checked(env, "WebView.internalLoadChanged", |env| {
                env.call_method(
                    web_view,
                    WEB_VIEW_INTERNAL_LOAD_CHANGED_NAME,
                    WEB_VIEW_INTERNAL_LOAD_CHANGED_SIG,
                    &[JValue::Int(state), JValue::Object(&url)],
                )?
                .v()
            })
        },
    )
    .is_some()
}

pub fn fire_web_view_progress_changed(java_vm: &JavaVM, widget: jlong, percent: i32) {
    fire_web_view_callback(
        java_vm,
        widget,
        "WebView.internalProgressChanged",
        move |env, web_view| {
            checked(env, "WebView.internalProgressChanged", |env| {
                env.call_method(
                    web_view,
                    WEB_VIEW_INTERNAL_PROGRESS_CHANGED_NAME,
                    WEB_VIEW_INTERNAL_PROGRESS_CHANGED_SIG,
                    &[JValue::Int(percent)],
                )?
                .v()
            })
        },
    );
}

pub fn fire_web_view_load_resource(java_vm: &JavaVM, widget: jlong, url: &str) {
    let url = url.to_string();
    fire_web_view_callback(
        java_vm,
        widget,
        "WebView.internalLoadResource",
        move |env, web_view| {
            let url = env.new_string(&url)?;
            checked(env, "WebView.internalLoadResource", |env| {
                env.call_method(
                    web_view,
                    WEB_VIEW_INTERNAL_LOAD_RESOURCE_NAME,
                    WEB_VIEW_INTERNAL_LOAD_RESOURCE_SIG,
                    &[JValue::Object(&url)],
                )?
                .v()
            })
        },
    );
}

fn new_web_resource_request<'local>(
    env: &mut Env<'local>,
    request: &crate::webview::client::NavigationRequest,
) -> Result<JObject<'local>, FrameworkError> {
    let url = env.new_string(&request.url)?;
    let method = env.new_string(&request.method)?;
    checked(env, "new EclipseWebResourceRequest", |env| {
        env.new_object(
            WEB_RESOURCE_REQUEST_CLASS,
            WEB_RESOURCE_REQUEST_CTOR_SIG,
            &[
                JValue::Object(&url),
                JValue::Object(&method),
                JValue::Bool(request.redirect),
                JValue::Bool(request.user_gesture),
            ],
        )
    })
}

pub fn fire_web_view_received_error(
    java_vm: &JavaVM,
    widget: jlong,
    url: &str,
    code: i32,
    description: &str,
) {
    let request = crate::webview::client::NavigationRequest {
        url: url.to_string(),
        method: "GET".to_string(),
        redirect: false,
        user_gesture: false,
    };
    let description = description.to_string();
    fire_web_view_callback(
        java_vm,
        widget,
        "WebView.internalReceivedError",
        move |env, web_view| {
            let request = new_web_resource_request(env, &request)?;
            let description = env.new_string(&description)?;
            let error = checked(env, "new WebResourceError", |env| {
                env.new_object(
                    WEB_RESOURCE_ERROR_CLASS,
                    WEB_RESOURCE_ERROR_CTOR_SIG,
                    &[JValue::Int(code), JValue::Object(&description)],
                )
            })?;
            checked(env, "WebView.internalReceivedError", |env| {
                env.call_method(
                    web_view,
                    WEB_VIEW_INTERNAL_RECEIVED_ERROR_NAME,
                    WEB_VIEW_INTERNAL_RECEIVED_ERROR_SIG,
                    &[JValue::Object(&request), JValue::Object(&error)],
                )?
                .v()
            })
        },
    );
}

pub fn fire_web_view_should_override_url_loading(
    java_vm: &JavaVM,
    widget: jlong,
    request: crate::webview::client::NavigationRequest,
) -> bool {
    fire_web_view_callback(
        java_vm,
        widget,
        "WebView.internalShouldOverrideUrlLoading",
        move |env, web_view| {
            let request = new_web_resource_request(env, &request)?;
            checked(env, "WebView.internalShouldOverrideUrlLoading", |env| {
                env.call_method(
                    web_view,
                    WEB_VIEW_INTERNAL_SHOULD_OVERRIDE_URL_LOADING_NAME,
                    WEB_VIEW_INTERNAL_SHOULD_OVERRIDE_URL_LOADING_SIG,
                    &[JValue::Object(&request)],
                )?
                .z()
            })
        },
    )
    .unwrap_or(false)
}

pub fn dispatch_activity_back(java_vm: &JavaVM) {
    let dispatched =
        dispatch_webview_callback_on_main(java_vm, "Activity.onBackPressed", |env: &mut Env| {
            dispatch_back_to_latest_activity(env)
        });
    match dispatched {
        Some(Ok(true)) => {}
        Some(Ok(false)) => tracing::warn!(
            target: "android.app.Activity",
            "a WebView window asked for Back but no Android Activity is live"
        ),
        Some(Err(error)) => tracing::warn!(
            target: "android.app.Activity",
            %error,
            "Back for a WebView window failed"
        ),
        None => tracing::warn!(
            target: "android.app.Activity",
            "Back for a WebView window did not complete"
        ),
    }
}

#[derive(Clone, Copy)]
enum DirectWebViewMode {
    Smoke,
    RobloxLogin,
}

pub fn drive_roblox_web_login(vm: &Vm) -> Result<jlong, FrameworkError> {
    drive_direct_webview(
        vm,
        "https://www.roblox.com/login",
        DirectWebViewMode::RobloxLogin,
    )
}

pub fn drive_webview_smoke(vm: &Vm, url: &str) -> Result<jlong, FrameworkError> {
    drive_direct_webview(vm, url, DirectWebViewMode::Smoke)
}

fn drive_direct_webview(
    vm: &Vm,
    url: &str,
    mode: DirectWebViewMode,
) -> Result<jlong, FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| direct_webview_inner(env, url, mode))) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

fn direct_webview_inner(
    env: &mut Env,
    url: &str,
    mode: DirectWebViewMode,
) -> Result<jlong, FrameworkError> {
    register_web_view_natives(env)?;

    register_cookie_manager_natives(env)?;
    let class = checked(env, "find_class android.webkit.WebView", |env| {
        env.find_class(WEB_VIEW_CLASS)
    })?;

    let webview = checked(env, "AllocObject android.webkit.WebView", |env| {
        env.alloc_object(&class)
    })?;
    let handle =
        view_registry::allocate("android.webkit.WebView").map_err(FrameworkError::ViewRegistry)?;

    let owner = env.new_global_ref(&webview)?;
    view_registry::set_jobject(handle, view_registry::ViewObject::Owned(owner))
        .map_err(FrameworkError::ViewRegistry)?;
    if matches!(mode, DirectWebViewMode::RobloxLogin) {
        view_registry::mark_window_root(handle).map_err(FrameworkError::ViewRegistry)?;
    }

    let long_sig = unsafe {
        FieldSignature::from_raw_parts(jni_str!("J"), JavaType::Primitive(Primitive::Long))
    };
    checked(env, "WebView.widget=", |env| {
        env.set_field(&webview, jni_str!("widget"), &long_sig, handle.into())
    })?;

    let (client_class_name, client_step) = match mode {
        DirectWebViewMode::Smoke => (
            jni_str!("android/webkit/EclipseWebViewClientProbe"),
            "EclipseWebViewClientProbe.<init>",
        ),
        DirectWebViewMode::RobloxLogin => (
            jni_str!("android/webkit/WebViewClient"),
            "WebViewClient.<init>",
        ),
    };
    let client_class = checked(env, "find WebViewClient class", |env| {
        env.find_class(client_class_name)
    })?;
    let client_obj = checked(env, client_step, |env| {
        env.new_object(&client_class, jni_sig!("()V"), &[])
    })?;
    checked(env, "WebView.setWebViewClient", |env| {
        env.call_method(
            &webview,
            jni_str!("setWebViewClient"),
            jni_sig!("(Landroid/webkit/WebViewClient;)V"),
            &[JValue::Object(&client_obj)],
        )?
        .v()
    })?;
    if matches!(mode, DirectWebViewMode::Smoke) {
        let probe_class = checked(env, "find_class EclipseBridgeProbe", |env| {
            env.find_class(jni_str!("android/webkit/EclipseBridgeProbe"))
        })?;
        let probe = checked(env, "EclipseBridgeProbe.<init>", |env| {
            env.new_object(&probe_class, jni_sig!("()V"), &[])
        })?;
        let iface_name = env.new_string("EclipseTest")?;
        checked(env, "WebView.addJavascriptInterface", |env| {
            env.call_method(
                &webview,
                jni_str!("addJavascriptInterface"),
                jni_sig!("(Ljava/lang/Object;Ljava/lang/String;)V"),
                &[JValue::Object(&probe), JValue::Object(&iface_name)],
            )?
            .v()
        })?;
    }

    let jurl = env.new_string(url)?;
    checked(env, "WebView.loadUrl", |env| {
        env.call_method(
            &webview,
            jni_str!("loadUrl"),
            jni_sig!("(Ljava/lang/String;)V"),
            &[JValue::Object(&jurl)],
        )?
        .v()
    })?;
    match mode {
        DirectWebViewMode::Smoke => tracing::info!(
            handle,
            "__webview-test: WebView.loadUrl driven through the production native path"
        ),
        DirectWebViewMode::RobloxLogin => tracing::info!(
            handle,
            "opened Roblox's first-party login page in Eclipse's persistent WebView profile"
        ),
    }
    Ok(handle)
}

const ECLIPSE_BRIDGE_PROBE_CLASS: &JNIStr = jni_str!("android/webkit/EclipseBridgeProbe");

fn object_field_sig() -> FieldSignature<'static> {
    unsafe { FieldSignature::from_raw_parts(jni_str!("Ljava/lang/Object;"), JavaType::Object) }
}

fn int_field_sig() -> FieldSignature<'static> {
    unsafe { FieldSignature::from_raw_parts(jni_str!("I"), JavaType::Primitive(Primitive::Int)) }
}

pub fn webview_evaluate(vm: &Vm, widget: jlong, script: &str) -> Result<(), FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| -> Result<(), FrameworkError> {
            let probe_class = checked(env, "find EclipseBridgeProbe", |env| {
                env.find_class(ECLIPSE_BRIDGE_PROBE_CLASS)
            })?;

            checked(env, "reset lastValue", |env| {
                env.set_static_field(
                    &probe_class,
                    jni_str!("lastValue"),
                    object_field_sig(),
                    JValue::Object(&JObject::null()),
                )
            })?;
            let probe = checked(env, "EclipseBridgeProbe.<init>", |env| {
                env.new_object(&probe_class, jni_sig!("()V"), &[])
            })?;
            let webview = match view_registry::local_jobject(env, widget) {
                Ok(Ok(Some(obj))) => obj,
                _ => {
                    return Err(FrameworkError::Jni(jni::errors::Error::NullPtr(
                        "no webview object",
                    )))
                }
            };
            let jscript = env.new_string(script)?;
            checked(env, "WebView.evaluateJavascript", |env| {
                env.call_method(
                    &webview,
                    jni_str!("evaluateJavascript"),
                    jni_sig!("(Ljava/lang/String;Landroid/webkit/ValueCallback;)V"),
                    &[JValue::Object(&jscript), JValue::Object(&probe)],
                )?
                .v()
            })?;
            Ok(())
        })) {
            Ok(r) => r,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

pub fn read_probe_last_value(vm: &Vm) -> Option<String> {
    read_probe_object_field(vm, jni_str!("lastValue"))
}

pub fn read_probe_last(vm: &Vm) -> Option<String> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return None;
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm
        .attach_current_thread(|env: &mut Env| -> Result<Option<String>, FrameworkError> {
            match std::panic::catch_unwind(AssertUnwindSafe(|| -> Option<String> {
                let class = env.find_class(ECLIPSE_BRIDGE_PROBE_CLASS).ok()?;

                let sig = unsafe {
                    FieldSignature::from_raw_parts(jni_str!("Ljava/lang/String;"), JavaType::Object)
                };
                let v = env
                    .get_static_field(&class, jni_str!("last"), &sig)
                    .ok()?
                    .l()
                    .ok()?;
                if v.is_null() {
                    None
                } else {
                    jstring_object_to_string(env, v).ok()
                }
            })) {
                Ok(r) => Ok(r),
                Err(_) => Err(FrameworkError::Panicked),
            }
        })
        .ok()
        .flatten()
}

fn read_probe_object_field(vm: &Vm, field: &JNIStr) -> Option<String> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return None;
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm
        .attach_current_thread(|env: &mut Env| -> Result<Option<String>, FrameworkError> {
            match std::panic::catch_unwind(AssertUnwindSafe(|| -> Option<String> {
                let class = env.find_class(ECLIPSE_BRIDGE_PROBE_CLASS).ok()?;
                let v = env
                    .get_static_field(&class, field, object_field_sig())
                    .ok()?
                    .l()
                    .ok()?;
                if v.is_null() {
                    None
                } else {
                    object_to_string(env, &v)
                }
            })) {
                Ok(r) => Ok(r),
                Err(_) => Err(FrameworkError::Panicked),
            }
        })
        .ok()
        .flatten()
}

pub fn cookie_manager_set_cookie(vm: &Vm, url: &str, value: &str) -> Result<(), FrameworkError> {
    with_cookie_manager(vm, |env, cm| {
        let jurl = env.new_string(url)?;
        let jval = env.new_string(value)?;
        checked(env, "CookieManager.setCookie", |env| {
            env.call_method(
                cm,
                jni_str!("setCookie"),
                jni_sig!("(Ljava/lang/String;Ljava/lang/String;)V"),
                &[JValue::Object(&jurl), JValue::Object(&jval)],
            )?
            .v()
        })
    })
}

pub fn cookie_manager_get_cookie(vm: &Vm, url: &str) -> String {
    with_cookie_manager(vm, |env, cm| {
        let jurl = env.new_string(url)?;
        let obj = checked(env, "CookieManager.getCookie", |env| {
            env.call_method(
                cm,
                jni_str!("getCookie"),
                jni_sig!("(Ljava/lang/String;)Ljava/lang/String;"),
                &[JValue::Object(&jurl)],
            )?
            .l()
        })?;
        if obj.is_null() {
            Ok(String::new())
        } else {
            jstring_object_to_string(env, obj).map_err(FrameworkError::Jni)
        }
    })
    .unwrap_or_default()
}

pub fn cookie_manager_flush(vm: &Vm) -> Result<(), FrameworkError> {
    with_cookie_manager(vm, |env, cm| {
        checked(env, "CookieManager.flush", |env| {
            env.call_method(cm, jni_str!("flush"), jni_sig!("()V"), &[])?
                .v()
        })
    })
}

pub fn cookie_manager_set_cookie_cb(vm: &Vm, url: &str, value: &str) -> Result<(), FrameworkError> {
    with_cookie_manager(vm, |env, cm| {
        let probe_class = env
            .find_class(ECLIPSE_BRIDGE_PROBE_CLASS)
            .map_err(FrameworkError::Jni)?;
        checked(env, "reset lastValue (cookie cb)", |env| {
            env.set_static_field(
                &probe_class,
                jni_str!("lastValue"),
                object_field_sig(),
                JValue::Object(&JObject::null()),
            )
        })?;
        let probe = checked(env, "EclipseBridgeProbe.<init> (cookie cb)", |env| {
            env.new_object(&probe_class, jni_sig!("()V"), &[])
        })?;
        let jurl = env.new_string(url)?;
        let jval = env.new_string(value)?;
        checked(env, "CookieManager.setCookie(3-arg)", |env| {
            env.call_method(
                cm,
                jni_str!("setCookie"),
                jni_sig!("(Ljava/lang/String;Ljava/lang/String;Landroid/webkit/ValueCallback;)V"),
                &[
                    JValue::Object(&jurl),
                    JValue::Object(&jval),
                    JValue::Object(&probe),
                ],
            )?
            .v()
        })
    })
}

fn with_cookie_manager<T>(
    vm: &Vm,
    f: impl FnOnce(&mut Env, &JObject) -> Result<T, FrameworkError>,
) -> Result<T, FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| -> Result<T, FrameworkError> {
            let cls = checked(env, "find_class CookieManager", |env| {
                env.find_class(COOKIE_MANAGER_CLASS)
            })?;
            let cm = checked(env, "new CookieManager", |env| {
                env.new_object(&cls, jni_sig!("()V"), &[])
            })?;
            f(env, &cm)
        })) {
            Ok(r) => r,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

const WEB_VIEW_NATIVE_EVALUATE_JAVASCRIPT_NAME: &JNIStr = jni_str!("native_evaluateJavascript");
const WEB_VIEW_NATIVE_EVALUATE_JAVASCRIPT_SIG: &JNIStr =
    jni_str!("(JLjava/lang/String;Landroid/webkit/ValueCallback;)V");

const WEB_VIEW_NATIVE_ADD_JAVASCRIPT_INTERFACE_NAME: &JNIStr =
    jni_str!("native_addJavascriptInterface");
const WEB_VIEW_NATIVE_ADD_JAVASCRIPT_INTERFACE_SIG: &JNIStr =
    jni_str!("(JLjava/lang/Object;Ljava/lang/String;)V");

pub const COOKIE_MANAGER_CLASS: &JNIStr = jni_str!("android/webkit/CookieManager");
const CM_NATIVE_GET_COOKIE_NAME: &JNIStr = jni_str!("native_getCookie");
const CM_NATIVE_GET_COOKIE_SIG: &JNIStr = jni_str!("(Ljava/lang/String;)Ljava/lang/String;");
const CM_NATIVE_SET_COOKIE_NAME: &JNIStr = jni_str!("native_setCookie");
const CM_NATIVE_SET_COOKIE_SIG: &JNIStr = jni_str!("(Ljava/lang/String;Ljava/lang/String;)V");
const CM_NATIVE_SET_COOKIE_CB_SIG: &JNIStr =
    jni_str!("(Ljava/lang/String;Ljava/lang/String;Landroid/webkit/ValueCallback;)V");
const CM_NATIVE_REMOVE_ALL_COOKIES_NAME: &JNIStr = jni_str!("native_removeAllCookies");
const CM_NATIVE_REMOVE_ALL_COOKIES_SIG: &JNIStr = jni_str!("(Landroid/webkit/ValueCallback;)V");
const CM_NATIVE_REMOVE_SESSION_COOKIES_NAME: &JNIStr = jni_str!("native_removeSessionCookies");
const CM_NATIVE_REMOVE_SESSION_COOKIES_SIG: &JNIStr = jni_str!("(Landroid/webkit/ValueCallback;)V");
const CM_NATIVE_FLUSH_NAME: &JNIStr = jni_str!("native_flush");
const CM_NATIVE_FLUSH_SIG: &JNIStr = jni_str!("()V");

const JAVASCRIPT_INTERFACE_CLASS: &JNIStr = jni_str!("android/webkit/JavascriptInterface");

struct BridgeMethodMeta {
    method: Global<JObject<'static>>,

    param_types: Vec<String>,

    return_type: String,
}

struct BridgeEntry {
    object: Global<JObject<'static>>,
    methods: std::collections::HashMap<String, Vec<BridgeMethodMeta>>,

    era: u64,
}

fn bridge_registry(
) -> &'static std::sync::Mutex<std::collections::HashMap<(jlong, String), BridgeEntry>> {
    static R: OnceLock<std::sync::Mutex<std::collections::HashMap<(jlong, String), BridgeEntry>>> =
        OnceLock::new();
    R.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

static WEBVIEW_BRIDGE_ENTRIES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

fn has_webview_bridges() -> bool {
    WEBVIEW_BRIDGE_ENTRIES.load(std::sync::atomic::Ordering::Relaxed) != 0
}

static WEBVIEW_CLOSE_ERA: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn webview_close_era() -> u64 {
    WEBVIEW_CLOSE_ERA.load(std::sync::atomic::Ordering::SeqCst)
}

pub fn bump_webview_close_era() -> u64 {
    WEBVIEW_CLOSE_ERA.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

type EvalCallbacks =
    std::sync::Mutex<std::collections::HashMap<u32, (jlong, u64, Global<JObject<'static>>)>>;

fn eval_callbacks() -> &'static EvalCallbacks {
    static R: OnceLock<EvalCallbacks> = OnceLock::new();
    R.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn cookie_set_callbacks(
) -> &'static std::sync::Mutex<std::collections::HashMap<u32, Global<JObject<'static>>>> {
    static R: OnceLock<std::sync::Mutex<std::collections::HashMap<u32, Global<JObject<'static>>>>> =
        OnceLock::new();
    R.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn cookie_clear_callbacks(
) -> &'static std::sync::Mutex<std::collections::HashMap<u32, Global<JObject<'static>>>> {
    static R: OnceLock<std::sync::Mutex<std::collections::HashMap<u32, Global<JObject<'static>>>>> =
        OnceLock::new();
    R.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn drop_bridges_for(widget: jlong) {
    if let Ok(mut reg) = bridge_registry().lock() {
        reg.retain(|(w, _), _| *w != widget);
        WEBVIEW_BRIDGE_ENTRIES.store(reg.len(), std::sync::atomic::Ordering::Relaxed);
    }
}

fn drop_bridge(widget: jlong, name: &str) {
    if let Ok(mut reg) = bridge_registry().lock() {
        reg.remove(&(widget, name.to_string()));
        WEBVIEW_BRIDGE_ENTRIES.store(reg.len(), std::sync::atomic::Ordering::Relaxed);
    }
}

pub fn drop_bridges_for_view_closed(widget: jlong, upto_era: u64) {
    if let Ok(mut reg) = bridge_registry().lock() {
        reg.retain(|(w, _), e| bridge_survives_view_close(*w, e.era, widget, upto_era));
        WEBVIEW_BRIDGE_ENTRIES.store(reg.len(), std::sync::atomic::Ordering::Relaxed);
    }
}

fn bridge_survives_view_close(
    entry_widget: jlong,
    entry_era: u64,
    closed: jlong,
    upto_era: u64,
) -> bool {
    entry_widget != closed || entry_era > upto_era
}

pub fn drop_all_bridges() {
    if let Ok(mut reg) = bridge_registry().lock() {
        reg.clear();
        WEBVIEW_BRIDGE_ENTRIES.store(0, std::sync::atomic::Ordering::Relaxed);
    }
}

extern "system" fn web_view_native_evaluate_javascript<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    script: JString<'local>,
    callback: JObject<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if view_registry::with_view(widget, |_v| ()).is_err() {
            tracing::warn!(
                target: "android.webkit.WebView",
                widget,
                "WebView.native_evaluateJavascript: invalid view handle (ignored)"
            );
            return Ok(());
        }
        let script_s = if script.is_null() {
            None
        } else {
            match script.try_to_string(env) {
                Ok(s) => Some(s),
                Err(_) => {
                    if env.exception_check() {
                        env.exception_describe();
                        env.exception_clear();
                    }
                    None
                }
            }
        };
        let Some(script_s) = script_s else {
            tracing::debug!(target: "android.webkit.WebView", widget, "evaluateJavascript: null/unreadable script (ignored)");
            return Ok(());
        };
        let request_id = crate::webview::client::next_request_id();

        if !callback.is_null() {
            match env.new_global_ref(&callback) {
                Ok(g) => {
                    note_non_main_callback_registrar("evaluateJavascript ValueCallback");
                    if let Ok(mut cbs) = eval_callbacks().lock() {
                        cbs.insert(request_id, (widget, webview_close_era(), g));
                    }
                }
                Err(_) => {
                    if env.exception_check() {
                        env.exception_describe();
                        env.exception_clear();
                    }
                }
            }
        }

        let degrade = |env: &mut Env| {
            if let Some((_w, _era, g)) = eval_callbacks()
                .lock()
                .ok()
                .and_then(|mut m| m.remove(&request_id))
            {
                match env.new_local_ref(g.as_obj()) {
                    Ok(local) => fire_string_value_callback(env, &local, "null"),
                    Err(_) => clear_pending(env),
                }
            }
        };
        if let Err(e) = crate::webview::client::evaluate_js(widget, request_id, script_s) {
            degrade(env);
            warn_bridge_unavailable("evaluateJavascript", widget, &e.to_string());
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn web_view_native_add_javascript_interface<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    object: JObject<'local>,
    name: JString<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if view_registry::with_view(widget, |_v| ()).is_err() {
            tracing::warn!(target: "android.webkit.WebView", widget, "addJavascriptInterface: invalid view handle (ignored)");
            return Ok(());
        }
        if object.is_null() {
            tracing::warn!(target: "android.webkit.WebView", widget, "addJavascriptInterface: null object (ignored)");
            return Ok(());
        }
        let iface = if name.is_null() {
            None
        } else {
            name.try_to_string(env).ok()
        };
        let Some(iface) = iface.filter(|s| !s.is_empty()) else {
            if env.exception_check() {
                env.exception_describe();
                env.exception_clear();
            }
            tracing::warn!(target: "android.webkit.WebView", widget, "addJavascriptInterface: null/empty name (ignored)");
            return Ok(());
        };

        let methods = match reflect_javascript_interface_methods(env, &object) {
            Ok(m) => m,
            Err(_) => {
                if env.exception_check() {
                    env.exception_describe();
                    env.exception_clear();
                }
                tracing::warn!(target: "android.webkit.WebView", widget, iface = %iface, "addJavascriptInterface: reflection failed (no bridge registered)");
                return Ok(());
            }
        };
        let object_global = match env.new_global_ref(&object) {
            Ok(g) => g,
            Err(_) => {
                if env.exception_check() {
                    env.exception_describe();
                    env.exception_clear();
                }
                return Ok(());
            }
        };

        let mut method_names: Vec<String> = methods.keys().cloned().collect();
        method_names.sort_unstable();
        let method_count = method_names.len();
        if let Ok(mut reg) = bridge_registry().lock() {
            reg.insert(
                (widget, iface.clone()),
                BridgeEntry {
                    object: object_global,
                    methods,
                    era: webview_close_era(),
                },
            );
            WEBVIEW_BRIDGE_ENTRIES.store(reg.len(), std::sync::atomic::Ordering::Relaxed);
        }
        match crate::webview::client::register_bridge(widget, iface.clone(), method_names) {
            Ok(()) => tracing::info!(
                target: "android.webkit.WebView",
                widget,
                iface = %iface,
                method_count,
                "addJavascriptInterface: registered @JavascriptInterface bridge (method names not logged)"
            ),
            Err(e) => warn_bridge_unavailable("addJavascriptInterface", widget, &e.to_string()),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

fn warn_bridge_unavailable(feature: &str, widget: jlong, reason: &str) {
    tracing::warn!(
        target: "android.webkit.WebView",
        widget,
        feature,
        reason,
        "WebView bridge feature unavailable — web engine helper degraded (honest no-op)"
    );
}

fn reflect_javascript_interface_methods(
    env: &mut Env,
    object: &JObject,
) -> jni::errors::Result<std::collections::HashMap<String, Vec<BridgeMethodMeta>>> {
    let anno_class = env.find_class(JAVASCRIPT_INTERFACE_CLASS)?;
    let class = env.get_object_class(object)?;

    let arr = env
        .call_method(
            &class,
            jni_str!("getMethods"),
            jni_sig!("()[Ljava/lang/reflect/Method;"),
            &[],
        )?
        .l()?;
    let methods: JObjectArray = env.cast_local::<JObjectArray>(arr)?;
    let n = methods.len(env)?;
    let mut out: std::collections::HashMap<String, Vec<BridgeMethodMeta>> =
        std::collections::HashMap::new();
    for i in 0..n {
        let entry: Option<(String, BridgeMethodMeta)> = env.with_local_frame(
            24,
            |env| -> jni::errors::Result<Option<(String, BridgeMethodMeta)>> {
                let m = methods.get_element(env, i)?;
                let present = env
                    .call_method(
                        &m,
                        jni_str!("isAnnotationPresent"),
                        jni_sig!("(Ljava/lang/Class;)Z"),
                        &[JValue::Object(&anno_class)],
                    )?
                    .z()?;
                if !present {
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
                let mname = jstring_object_to_string(env, name_obj)?;
                let ret_obj = env
                    .call_method(
                        &m,
                        jni_str!("getReturnType"),
                        jni_sig!("()Ljava/lang/Class;"),
                        &[],
                    )?
                    .l()?;
                let return_type = class_get_name(env, ret_obj)?;
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
                let mut param_types = Vec::with_capacity(pn);
                for j in 0..pn {
                    let p = params.get_element(env, j)?;
                    param_types.push(class_get_name(env, p)?);
                }
                let method = env.new_global_ref(&m)?;
                Ok(Some((
                    mname,
                    BridgeMethodMeta {
                        method,
                        param_types,
                        return_type,
                    },
                )))
            },
        )?;
        if let Some((name, meta)) = entry {
            out.entry(name).or_default().push(meta);
        }
    }
    Ok(out)
}

fn class_get_name(env: &mut Env, class_obj: JObject) -> jni::errors::Result<String> {
    let name_obj = env
        .call_method(
            &class_obj,
            jni_str!("getName"),
            jni_sig!("()Ljava/lang/String;"),
            &[],
        )?
        .l()?;
    jstring_object_to_string(env, name_obj)
}

fn jstring_object_to_string(env: &mut Env, obj: JObject) -> jni::errors::Result<String> {
    let jstr: JString = env.cast_local::<JString>(obj)?;
    let chars = jstr.mutf8_chars(env)?;
    Ok(String::from(chars))
}

extern "system" fn web_view_cookie_manager_get_cookie<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    url: JString<'local>,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        let url_s = read_jstring(env, &url).unwrap_or_default();
        let fixed = fixup_webview_cookie_url(&url_s);
        let cookies = crate::webview::client::cookie_get(fixed.url).unwrap_or_else(|error| {
            tracing::warn!(
                target: "android.webkit.CookieManager",
                %error,
                "CookieManager.getCookie failed; reporting no cookies"
            );
            Vec::new()
        });
        env.new_string(format_cookies(&cookies))
    })
    .resolve::<LogErrorAndDefault>()
}

fn send_set_cookie(request_id: u32, url: &str, header: String) -> Option<bool> {
    let (url, header) = set_cookie_request(url, header);
    match crate::webview::client::cookie_set(request_id, url, header) {
        Ok(crate::webview::client::CookieAnswer::Now(ok)) => Some(ok),
        Ok(crate::webview::client::CookieAnswer::FromHelper) => None,
        Err(error) => {
            tracing::warn!(
                target: "android.webkit.CookieManager",
                %error,
                "CookieManager.setCookie failed"
            );
            Some(false)
        }
    }
}

fn post_cookie_answer(
    env: &mut Env,
    callbacks: &std::sync::Mutex<std::collections::HashMap<u32, Global<JObject<'static>>>>,
    request_id: u32,
    value: bool,
) {
    if let Some(g) = callbacks
        .lock()
        .ok()
        .and_then(|mut m| m.remove(&request_id))
    {
        post_webview_callback_to_main(
            env,
            "ValueCallback.onReceiveValue(Boolean)",
            boolean_callback_job(g, value),
        );
    }
}

extern "system" fn web_view_cookie_manager_set_cookie<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    url: JString<'local>,
    value: JString<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let (Some(url_s), Some(value_s)) = (read_jstring(env, &url), read_jstring(env, &value))
        else {
            return Ok(());
        };
        let request_id = crate::webview::client::next_request_id();
        send_set_cookie(request_id, &url_s, value_s);
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn web_view_cookie_manager_set_cookie_cb<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    url: JString<'local>,
    value: JString<'local>,
    callback: JObject<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let (Some(url_s), Some(value_s)) = (read_jstring(env, &url), read_jstring(env, &value))
        else {
            return Ok(());
        };
        let request_id = crate::webview::client::next_request_id();
        if !callback.is_null() {
            if let Ok(g) = env.new_global_ref(&callback) {
                note_non_main_callback_registrar("setCookie(3-arg) ValueCallback");
                if let Ok(mut cbs) = cookie_set_callbacks().lock() {
                    cbs.insert(request_id, g);
                }
            }
        }
        if let Some(ok) = send_set_cookie(request_id, &url_s, value_s) {
            post_cookie_answer(env, cookie_set_callbacks(), request_id, ok);
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn web_view_cookie_manager_remove_all_cookies<'local>(
    env: EnvUnowned<'local>,
    _this: JObject<'local>,
    callback: JObject<'local>,
) {
    web_view_cookie_manager_remove_impl(env, callback, crate::webview::proto::ClearScope::All)
}

extern "system" fn web_view_cookie_manager_remove_session_cookies<'local>(
    env: EnvUnowned<'local>,
    _this: JObject<'local>,
    callback: JObject<'local>,
) {
    web_view_cookie_manager_remove_impl(env, callback, crate::webview::proto::ClearScope::Session)
}

fn web_view_cookie_manager_remove_impl<'local>(
    mut env: EnvUnowned<'local>,
    callback: JObject<'local>,
    scope: crate::webview::proto::ClearScope,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let request_id = crate::webview::client::next_request_id();
        if !callback.is_null() {
            if let Ok(g) = env.new_global_ref(&callback) {
                note_non_main_callback_registrar("removeAll/SessionCookies ValueCallback");
                if let Ok(mut cbs) = cookie_clear_callbacks().lock() {
                    cbs.insert(request_id, g);
                }
            }
        }
        let removed = match crate::webview::client::cookies_clear(request_id, scope) {
            Ok(crate::webview::client::CookieAnswer::Now(removed)) => Some(removed),
            Ok(crate::webview::client::CookieAnswer::FromHelper) => None,
            Err(error) => {
                tracing::warn!(
                    target: "android.webkit.CookieManager",
                    %error,
                    "CookieManager.remove*Cookies failed"
                );
                Some(false)
            }
        };
        if let Some(removed) = removed {
            post_cookie_answer(env, cookie_clear_callbacks(), request_id, removed);
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn web_view_cookie_manager_flush<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
) {
    env.with_env(|_env: &mut Env| -> jni::errors::Result<()> {
        match crate::webview::client::cookie_flush() {
            Ok(true) => {}
            Ok(false) => tracing::warn!(
                target: "android.webkit.CookieManager",
                "CookieManager.flush(): the cookies could not be saved"
            ),
            Err(error) => tracing::warn!(
                target: "android.webkit.CookieManager",
                %error,
                "CookieManager.flush(): cookies may not survive an immediate process exit"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

pub fn register_cookie_manager(vm: &Vm) -> Result<(), FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| register_cookie_manager_natives(env))) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

fn register_cookie_manager_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let bindings: [NativeBinding; 6] = [
        (
            CM_NATIVE_GET_COOKIE_NAME,
            CM_NATIVE_GET_COOKIE_SIG,
            web_view_cookie_manager_get_cookie as *mut c_void,
        ),
        (
            CM_NATIVE_SET_COOKIE_NAME,
            CM_NATIVE_SET_COOKIE_SIG,
            web_view_cookie_manager_set_cookie as *mut c_void,
        ),
        (
            CM_NATIVE_SET_COOKIE_NAME,
            CM_NATIVE_SET_COOKIE_CB_SIG,
            web_view_cookie_manager_set_cookie_cb as *mut c_void,
        ),
        (
            CM_NATIVE_REMOVE_ALL_COOKIES_NAME,
            CM_NATIVE_REMOVE_ALL_COOKIES_SIG,
            web_view_cookie_manager_remove_all_cookies as *mut c_void,
        ),
        (
            CM_NATIVE_REMOVE_SESSION_COOKIES_NAME,
            CM_NATIVE_REMOVE_SESSION_COOKIES_SIG,
            web_view_cookie_manager_remove_session_cookies as *mut c_void,
        ),
        (
            CM_NATIVE_FLUSH_NAME,
            CM_NATIVE_FLUSH_SIG,
            web_view_cookie_manager_flush as *mut c_void,
        ),
    ];
    let bound = register_class_natives_best_effort(env, COOKIE_MANAGER_CLASS, &bindings)?;
    tracing::info!(
        bound,
        "registered Eclipse's non-GTK backing for the android.webkit.CookieManager native surface (get/set/set-with-callback/removeAll/removeSession/flush → Eclipse's cookie jar, or the web engine helper while it runs) (per-method best-effort)"
    );
    Ok(())
}

fn read_jstring<'local>(env: &mut Env<'local>, s: &JString<'local>) -> Option<String> {
    if s.is_null() {
        return None;
    }
    match s.try_to_string(env) {
        Ok(v) => Some(v),
        Err(_) => {
            if env.exception_check() {
                env.exception_describe();
                env.exception_clear();
            }
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CookieUrlFixup {
    url: String,

    implied_domain: Option<String>,
}

fn fixup_webview_cookie_url(input: &str) -> CookieUrlFixup {
    if input.contains("://") || input.is_empty() {
        return CookieUrlFixup {
            url: input.to_string(),
            implied_domain: None,
        };
    }

    let (authority, suffix) = input.find('/').map_or((input, ""), |at| input.split_at(at));
    let (host_port, leading_dot) = match authority.strip_prefix('.') {
        Some(host) if !host.is_empty() => (host, true),
        _ => (authority, false),
    };
    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            (host, Some(port))
        }
        _ => (host_port, None),
    };
    let host_ok = !host.is_empty()
        && !host.starts_with('.')
        && !host.ends_with('.')
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
    if !host_ok || suffix.bytes().any(|b| b.is_ascii_control() || b == b' ') {
        return CookieUrlFixup {
            url: input.to_string(),
            implied_domain: None,
        };
    }
    let scheme = if port == Some("443") { "https" } else { "http" };
    let path = if suffix.is_empty() { "/" } else { suffix };
    CookieUrlFixup {
        url: format!("{scheme}://{host_port}{path}"),
        implied_domain: leading_dot.then(|| format!(".{host}")),
    }
}

fn set_cookie_has_attribute(header: &str, name: &str) -> bool {
    header.split(';').skip(1).any(|attribute| {
        attribute
            .split('=')
            .next()
            .is_some_and(|key| key.trim().eq_ignore_ascii_case(name))
    })
}

fn set_cookie_request(url: &str, header: String) -> (String, String) {
    let fixed = fixup_webview_cookie_url(url);
    let header = match fixed.implied_domain {
        Some(domain) if !set_cookie_has_attribute(&header, "domain") => {
            format!("{header}; Domain={domain}")
        }
        _ => header,
    };
    (fixed.url, header)
}

fn bridge_arg_lens(args: &[serde_json::Value]) -> Vec<usize> {
    args.iter()
        .map(|v| serde_json::to_string(v).map(|s| s.len()).unwrap_or(0))
        .collect()
}

fn bridge_identifier_for_log(s: &str) -> &str {
    let mut chars = s.chars();
    let identifier_shaped = match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {
            s.len() <= 64 && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
        }
        _ => false,
    };
    if identifier_shaped {
        s
    } else {
        "<non-identifier>"
    }
}

fn format_cookies(cookies: &[crate::webview::proto::CookiePair]) -> String {
    cookies
        .iter()
        .map(|c| format!("{}={}", c.name, c.value))
        .collect::<Vec<_>>()
        .join("; ")
}

#[derive(Debug, PartialEq, Eq)]
enum ArgKind {
    Str,
    IntBox,
    LongBox,
    ShortBox,
    ByteBox,
    FloatBox,
    DoubleBox,
    BoolBox,
    Null,

    Reject,
}

fn plan_arg(value: &serde_json::Value, param_type: &str) -> ArgKind {
    use serde_json::Value;
    let string_ok = matches!(
        param_type,
        "java.lang.String" | "java.lang.CharSequence" | "java.lang.Object"
    );
    match value {
        Value::Null => match param_type {
            "int" | "long" | "short" | "byte" | "float" | "double" | "boolean" | "char" => {
                ArgKind::Reject
            }
            _ => ArgKind::Null,
        },
        Value::String(_) => {
            if string_ok {
                ArgKind::Str
            } else {
                ArgKind::Reject
            }
        }
        Value::Bool(_) => match param_type {
            "boolean" | "java.lang.Boolean" | "java.lang.Object" => ArgKind::BoolBox,
            _ => ArgKind::Reject,
        },
        Value::Number(_) => match param_type {
            "int" | "java.lang.Integer" => ArgKind::IntBox,
            "long" | "java.lang.Long" => ArgKind::LongBox,
            "short" | "java.lang.Short" => ArgKind::ShortBox,
            "byte" | "java.lang.Byte" => ArgKind::ByteBox,
            "float" | "java.lang.Float" => ArgKind::FloatBox,
            "double" | "java.lang.Double" | "java.lang.Number" | "java.lang.Object" => {
                ArgKind::DoubleBox
            }
            _ => ArgKind::Reject,
        },

        Value::Array(_) | Value::Object(_) => ArgKind::Reject,
    }
}

fn select_overload_index(arities: &[usize], argc: usize) -> Option<usize> {
    arities.iter().position(|&a| a == argc)
}

fn number_or_quote(s: &str) -> String {
    if serde_json::from_str::<serde_json::Number>(s).is_ok() {
        s.to_string()
    } else {
        serde_json::to_string(s).unwrap_or_else(|_| "null".to_string())
    }
}

static BRIDGE_CALL_LOOPER_NOTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn note_first_bridge_call_thread() {
    if !BRIDGE_CALL_LOOPER_NOTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!(
            target: "android.webkit.WebView",
            thread = std::thread::current().name().unwrap_or("eclipse-webview-upcall"),
            "FIRST page bridge call reached Eclipse — it runs on the Looper-less upcall thread \
             (AOSP's thread IDENTITY is correct here; only the Looper is missing — web-engine M6 \
             row 2, deferred 2026-07-16). If the following line is a described \"Can't create \
             handler…\" throw, the deferral's trigger has fired: build the drained HandlerThread \
             analogue on this thread."
        );
    }
}

pub fn fire_bridge_call(
    java_vm: &JavaVM,
    widget: jlong,
    call_id: u32,
    payload_json: &str,
) -> (bool, String) {
    note_first_bridge_call_thread();
    let result: Result<(bool, String), FrameworkError> =
        java_vm.attach_current_thread(|env: &mut Env| {
            match std::panic::catch_unwind(AssertUnwindSafe(|| {
                fire_bridge_call_inner(env, widget, call_id, payload_json)
            })) {
                Ok(pair) => Ok(pair),
                Err(_) => Err(FrameworkError::Panicked),
            }
        });
    match result {
        Ok(pair) => pair,
        Err(_) => (false, "null".to_string()),
    }
}

fn fire_bridge_call_inner(
    env: &mut Env,
    widget: jlong,
    call_id: u32,
    payload_json: &str,
) -> (bool, String) {
    const REJECT: &str = "null";
    let parsed: serde_json::Value = match serde_json::from_str(payload_json) {
        Ok(v) => v,
        Err(_) => return (false, "\"eclipse: bad bridge payload\"".to_string()),
    };
    let (Some(iface), Some(method)) = (
        parsed.get("iface").and_then(|v| v.as_str()),
        parsed.get("method").and_then(|v| v.as_str()),
    ) else {
        return (false, "\"eclipse: bad bridge payload\"".to_string());
    };
    let args: Vec<serde_json::Value> = parsed
        .get("args")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    tracing::info!(
        target: "android.webkit.WebView",
        widget,
        call_id,
        iface = bridge_identifier_for_log(iface),
        method = bridge_identifier_for_log(method),
        args = args.len(),
        arg_lens = ?bridge_arg_lens(&args),
        "bridge call received (arg values not logged)"
    );

    let snapshot = {
        let reg = match bridge_registry().lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        match reg.get(&(widget, iface.to_string())) {
            Some(entry) => match entry.methods.get(method) {
                Some(overloads) => {
                    let arities: Vec<usize> =
                        overloads.iter().map(|m| m.param_types.len()).collect();
                    match select_overload_index(&arities, args.len()) {
                        Some(i) => {
                            let meta = &overloads[i];
                            match (
                                env.new_local_ref(entry.object.as_obj()),
                                env.new_local_ref(meta.method.as_obj()),
                            ) {
                                (Ok(o), Ok(m)) => Ok(Some((
                                    o,
                                    m,
                                    meta.param_types.clone(),
                                    meta.return_type.clone(),
                                ))),
                                _ => Ok(None),
                            }
                        }
                        None => Err(arities),
                    }
                }
                None => Ok(None),
            },
            None => Ok(None),
        }
    };
    let snapshot = match snapshot {
        Ok(s) => s,
        Err(arities) => {
            tracing::warn!(
                target: "android.webkit.WebView",
                widget,
                call_id,
                expected_arities = ?arities,
                got = args.len(),
                "bridge call arity matches no overload (rejected)"
            );
            return (false, REJECT.to_string());
        }
    };
    let Some((obj_local, method_local, param_types, return_type)) = snapshot else {
        tracing::debug!(target: "android.webkit.WebView", widget, call_id, "bridge call for an unregistered/unknown method (rejected, never fabricated)");
        return (false, REJECT.to_string());
    };

    let obj_class = match env.find_class(jni_str!("java/lang/Object")) {
        Ok(c) => c,
        Err(_) => {
            clear_pending(env);
            return (false, REJECT.to_string());
        }
    };
    let array = match env.new_object_array(param_types.len() as i32, &obj_class, JObject::null()) {
        Ok(a) => a,
        Err(_) => {
            clear_pending(env);
            return (false, REJECT.to_string());
        }
    };
    for (idx, (v, pt)) in args.iter().zip(param_types.iter()).enumerate() {
        if let Err(reason) = set_bridge_arg(env, &array, idx, v, pt) {
            tracing::warn!(
                target: "android.webkit.WebView",
                widget,
                call_id,
                method,
                param_type = %pt,
                reason,
                "bridge arg marshal rejected (unsupported/mismatched type)"
            );
            return (false, REJECT.to_string());
        }
    }

    let invoke = env.call_method(
        &method_local,
        jni_str!("invoke"),
        jni_sig!("(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;"),
        &[JValue::Object(&obj_local), JValue::Object(&array)],
    );
    let result_obj = match invoke.and_then(|v| v.l()) {
        Ok(o) => o,
        Err(_) => {
            clear_pending(env);
            tracing::warn!(target: "android.webkit.WebView", widget, call_id, method, "bridge method invocation threw (rejected)");
            return (false, "\"eclipse: bridge invocation error\"".to_string());
        }
    };
    let result_json = marshal_bridge_return(env, &result_obj, &return_type);
    (true, result_json)
}

fn set_bridge_arg<'local>(
    env: &mut Env<'local>,
    array: &JObjectArray<'local>,
    idx: usize,
    v: &serde_json::Value,
    param_type: &str,
) -> Result<(), &'static str> {
    macro_rules! set {
        ($obj:expr) => {{
            array
                .set_element(env, idx, $obj)
                .map_err(|_| "set-array-element")?;
        }};
    }
    macro_rules! box_num {
        ($class:literal, $sig:literal, $jv:expr) => {{
            let boxed = env
                .call_static_method(
                    jni_str!($class),
                    jni_str!("valueOf"),
                    jni_sig!($sig),
                    &[$jv],
                )
                .and_then(|r| r.l())
                .map_err(|_| {
                    clear_pending(env);
                    "box-number"
                })?;
            set!(&boxed);
        }};
    }
    let num = v.as_f64();
    match plan_arg(v, param_type) {
        ArgKind::Reject => return Err("unsupported-type"),
        ArgKind::Null => {}
        ArgKind::Str => {
            let s = v.as_str().unwrap_or_default();
            let js = env.new_string(s).map_err(|_| {
                clear_pending(env);
                "string-alloc"
            })?;
            set!(&js);
        }
        ArgKind::IntBox => box_num!(
            "java/lang/Integer",
            "(I)Ljava/lang/Integer;",
            JValue::Int(num.unwrap_or(0.0) as jint)
        ),
        ArgKind::LongBox => box_num!(
            "java/lang/Long",
            "(J)Ljava/lang/Long;",
            JValue::Long(num.unwrap_or(0.0) as jlong)
        ),
        ArgKind::ShortBox => box_num!(
            "java/lang/Short",
            "(S)Ljava/lang/Short;",
            JValue::Short(num.unwrap_or(0.0) as jshort)
        ),
        ArgKind::ByteBox => box_num!(
            "java/lang/Byte",
            "(B)Ljava/lang/Byte;",
            JValue::Byte(num.unwrap_or(0.0) as i8)
        ),
        ArgKind::FloatBox => box_num!(
            "java/lang/Float",
            "(F)Ljava/lang/Float;",
            JValue::Float(num.unwrap_or(0.0) as jfloat)
        ),
        ArgKind::DoubleBox => box_num!(
            "java/lang/Double",
            "(D)Ljava/lang/Double;",
            JValue::Double(num.unwrap_or(0.0))
        ),
        ArgKind::BoolBox => box_num!(
            "java/lang/Boolean",
            "(Z)Ljava/lang/Boolean;",
            JValue::Bool(v.as_bool().unwrap_or(false))
        ),
    }
    Ok(())
}

fn marshal_bridge_return(env: &mut Env, result_obj: &JObject, return_type: &str) -> String {
    if result_obj.is_null() {
        return "null".to_string();
    }
    match return_type {
        "boolean" | "java.lang.Boolean" => {
            match env
                .call_method(result_obj, jni_str!("booleanValue"), jni_sig!("()Z"), &[])
                .and_then(|v| v.z())
            {
                Ok(true) => "true".to_string(),
                Ok(false) => "false".to_string(),
                Err(_) => {
                    clear_pending(env);
                    "null".to_string()
                }
            }
        }
        "int"
        | "long"
        | "short"
        | "byte"
        | "float"
        | "double"
        | "char"
        | "java.lang.Integer"
        | "java.lang.Long"
        | "java.lang.Short"
        | "java.lang.Byte"
        | "java.lang.Float"
        | "java.lang.Double"
        | "java.lang.Number"
        | "java.lang.Character" => match object_to_string(env, result_obj) {
            Some(s) => number_or_quote(&s),
            None => "null".to_string(),
        },

        _ => match object_to_string(env, result_obj) {
            Some(s) => serde_json::to_string(&s).unwrap_or_else(|_| "null".to_string()),
            None => "null".to_string(),
        },
    }
}

fn object_to_string(env: &mut Env, obj: &JObject) -> Option<String> {
    let s = env
        .call_method(
            obj,
            jni_str!("toString"),
            jni_sig!("()Ljava/lang/String;"),
            &[],
        )
        .and_then(|v| v.l());
    match s {
        Ok(str_obj) if !str_obj.is_null() => jstring_object_to_string(env, str_obj).ok(),
        _ => {
            clear_pending(env);
            None
        }
    }
}

static NON_MAIN_REGISTRAR_NOTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn note_non_main_callback_registrar(what: &'static str) {
    let main = MAIN_THREAD_ID.get();
    if main.is_some()
        && main != Some(&std::thread::current().id())
        && !NON_MAIN_REGISTRAR_NOTED.swap(true, std::sync::atomic::Ordering::Relaxed)
    {
        tracing::warn!(
            target: "android.webkit.WebView",
            registered_by = what,
            "an app-facing WebView ValueCallback was registered from a NON-main thread — Eclipse \
             will fire it on main (it pumps only the main Looper), which diverges from AOSP's \
             \"the caller's Looper\" for setCookie. Recorded divergence, 2026-07-16; logged once."
        );
    }
}

fn clear_pending(env: &mut Env) {
    if env.exception_check() {
        env.exception_describe();
        env.exception_clear();
    }
}

pub fn fire_evaluate_js_result(java_vm: &JavaVM, request_id: u32, ok: bool, value_json: &str) {
    let value = if ok { value_json } else { "null" };
    let global = eval_callbacks()
        .lock()
        .ok()
        .and_then(|mut m| m.remove(&request_id))
        .map(|(_widget, _era, g)| g);
    let Some(global) = global else {
        tracing::debug!(
            request_id,
            "value-callback result with no retained callback (fire-and-forget)"
        );
        return;
    };
    fire_string_callback_global(java_vm, global, value);
}

pub fn drain_eval_callbacks_for_view(java_vm: &JavaVM, widget: jlong, upto_era: u64) {
    let drained: Vec<(u32, Global<JObject<'static>>)> = match eval_callbacks().lock() {
        Ok(mut m) => {
            let ids = eval_drain_victims(&m, widget, upto_era);
            ids.into_iter()
                .filter_map(|id| m.remove(&id).map(|(_w, _era, g)| (id, g)))
                .collect()
        }
        Err(_) => Vec::new(),
    };
    if drained.is_empty() {
        return;
    }
    tracing::warn!(
        target: "android.webkit.WebView",
        widget,
        pending = drained.len(),
        "WebView closed with evaluateJavascript results still in flight — failing each \
         ValueCallback honestly (onReceiveValue(\"null\"))"
    );
    for (_id, g) in drained {
        fire_string_callback_global(java_vm, g, "null");
    }
}

fn eval_drain_victims<V>(
    m: &std::collections::HashMap<u32, (jlong, u64, V)>,
    widget: jlong,
    upto_era: u64,
) -> Vec<u32> {
    m.iter()
        .filter(|(_, (w, era, _))| *w == widget && *era <= upto_era)
        .map(|(id, _)| *id)
        .collect()
}

pub fn fail_webview_callbacks(java_vm: &JavaVM, request_ids: &[u32], reason: &str) {
    fn take<V>(
        m: &'static std::sync::Mutex<std::collections::HashMap<u32, V>>,
        request_ids: &[u32],
    ) -> Vec<V> {
        m.lock()
            .ok()
            .map(|mut m| request_ids.iter().filter_map(|id| m.remove(id)).collect())
            .unwrap_or_default()
    }
    let evals: Vec<(jlong, u64, Global<JObject<'static>>)> = take(eval_callbacks(), request_ids);
    let sets: Vec<Global<JObject<'static>>> = take(cookie_set_callbacks(), request_ids);
    let clears: Vec<Global<JObject<'static>>> = take(cookie_clear_callbacks(), request_ids);
    if evals.is_empty() && sets.is_empty() && clears.is_empty() {
        return;
    }
    tracing::warn!(
        target: "android.webkit.WebView",
        reason,
        eval_pending = evals.len(),
        cookie_set_pending = sets.len(),
        cookie_clear_pending = clears.len(),
        "web engine helper gone with ValueCallbacks still in flight — failing each honestly"
    );
    for (_widget, _era, g) in evals {
        fire_string_callback_global(java_vm, g, "null");
    }
    for g in sets {
        fire_boolean_callback_global(java_vm, g, false);
    }
    for g in clears {
        fire_boolean_callback_global(java_vm, g, false);
    }
}

pub fn fire_cookie_set_result(java_vm: &JavaVM, request_id: u32, ok: bool) {
    fire_boolean_result(java_vm, cookie_set_callbacks(), request_id, ok);
}

pub fn fire_cookies_clear_result(java_vm: &JavaVM, request_id: u32, removed: bool) {
    fire_boolean_result(java_vm, cookie_clear_callbacks(), request_id, removed);
}

fn fire_string_callback_global(java_vm: &JavaVM, global: Global<JObject<'static>>, value: &str) {
    let value = value.to_string();
    let _ = dispatch_webview_callback_on_main(
        java_vm,
        "ValueCallback.onReceiveValue(String)",
        move |env: &mut Env| match env.new_local_ref(global.as_obj()) {
            Ok(local) => fire_string_value_callback(env, &local, &value),
            Err(_) => clear_pending(env),
        },
    );
}

fn boolean_callback_job(
    global: Global<JObject<'static>>,
    ok: bool,
) -> impl for<'l> FnOnce(&mut Env<'l>) + Send + 'static {
    move |env: &mut Env| match env.new_local_ref(global.as_obj()) {
        Ok(local) => fire_boolean_value_callback(env, &local, ok),
        Err(_) => clear_pending(env),
    }
}

fn fire_boolean_callback_global(java_vm: &JavaVM, global: Global<JObject<'static>>, ok: bool) {
    let _ = dispatch_webview_callback_on_main(
        java_vm,
        "ValueCallback.onReceiveValue(Boolean)",
        boolean_callback_job(global, ok),
    );
}

fn fire_boolean_result(
    java_vm: &JavaVM,
    registry: &'static std::sync::Mutex<std::collections::HashMap<u32, Global<JObject<'static>>>>,
    request_id: u32,
    ok: bool,
) {
    let global = registry.lock().ok().and_then(|mut m| m.remove(&request_id));
    let Some(global) = global else {
        tracing::debug!(
            request_id,
            "boolean value-callback with no retained callback"
        );
        return;
    };
    fire_boolean_callback_global(java_vm, global, ok);
}

fn fire_string_value_callback(env: &mut Env, callback: &JObject, value: &str) {
    let jstr = match env.new_string(value) {
        Ok(s) => s,
        Err(_) => {
            clear_pending(env);
            return;
        }
    };
    if let Err(e) = checked(env, "ValueCallback.onReceiveValue(String)", |env| {
        env.call_method(
            callback,
            jni_str!("onReceiveValue"),
            jni_sig!("(Ljava/lang/Object;)V"),
            &[JValue::Object(&jstr)],
        )?
        .v()
    }) {
        tracing::debug!(error = %e, "onReceiveValue(String) threw (cleared)");
    }
}

fn fire_boolean_value_callback(env: &mut Env, callback: &JObject, ok: bool) {
    let boxed = env
        .call_static_method(
            jni_str!("java/lang/Boolean"),
            jni_str!("valueOf"),
            jni_sig!("(Z)Ljava/lang/Boolean;"),
            &[JValue::Bool(ok)],
        )
        .and_then(|v| v.l());
    let boxed = match boxed {
        Ok(b) => b,
        Err(_) => {
            clear_pending(env);
            return;
        }
    };
    if let Err(e) = checked(env, "ValueCallback.onReceiveValue(Boolean)", |env| {
        env.call_method(
            callback,
            jni_str!("onReceiveValue"),
            jni_sig!("(Ljava/lang/Object;)V"),
            &[JValue::Object(&boxed)],
        )?
        .v()
    }) {
        tracing::debug!(error = %e, "onReceiveValue(Boolean) threw (cleared)");
    }
}

const WIDGET_NATIVE_SET_TEXT_NAME: &JNIStr = jni_str!("native_setText");
const WIDGET_NATIVE_SET_TEXT_SIG: &JNIStr = jni_str!("(JLjava/lang/String;)V");

const RADIO_BUTTON_SET_TEXT_NAME: &JNIStr = jni_str!("setText");
const RADIO_BUTTON_SET_TEXT_SIG: &JNIStr = jni_str!("(Ljava/lang/CharSequence;)V");

const PROGRESS_BAR_SET_INDETERMINATE_NAME: &JNIStr = jni_str!("native_setIndeterminate");
const PROGRESS_BAR_SET_INDETERMINATE_SIG: &JNIStr = jni_str!("(Z)V");

const PROGRESS_NATIVE_SET_PROGRESS_NAME: &JNIStr = jni_str!("native_setProgress");
const PROGRESS_NATIVE_SET_PROGRESS_SIG: &JNIStr = jni_str!("(JF)V");

const SEEK_BAR_SET_MAX_NAME: &JNIStr = jni_str!("native_setMax");
const SEEK_BAR_SET_MAX_SIG: &JNIStr = jni_str!("(JI)V");

const BUTTON_SET_COMPOUND_DRAWABLES_NAME: &JNIStr = jni_str!("native_setCompoundDrawables");
const BUTTON_SET_COMPOUND_DRAWABLES_SIG: &JNIStr = jni_str!("(JJ)V");

const SPINNER_SET_ADAPTER_NAME: &JNIStr = jni_str!("native_setAdapter");
const SPINNER_SET_ADAPTER_SIG: &JNIStr = jni_str!("(JLandroid/widget/SpinnerAdapter;)V");

const EDIT_TEXT_ADD_TEXT_CHANGED_LISTENER_NAME: &JNIStr = jni_str!("native_addTextChangedListener");
const EDIT_TEXT_REMOVE_TEXT_CHANGED_LISTENER_NAME: &JNIStr =
    jni_str!("native_removeTextChangedListener");
const EDIT_TEXT_TEXT_CHANGED_LISTENER_SIG: &JNIStr = jni_str!("(JLandroid/text/TextWatcher;)V");
const EDIT_TEXT_SET_ON_EDITOR_ACTION_LISTENER_NAME: &JNIStr =
    jni_str!("native_setOnEditorActionListener");
const EDIT_TEXT_SET_ON_EDITOR_ACTION_LISTENER_SIG: &JNIStr =
    jni_str!("(JLandroid/widget/TextView$OnEditorActionListener;)V");

const EDIT_TEXT_GET_TEXT_NAME: &JNIStr = jni_str!("native_getText");
const EDIT_TEXT_GET_TEXT_SIG: &JNIStr = jni_str!("(J)Ljava/lang/String;");
const EDIT_TEXT_GET_SELECTION_START_NAME: &JNIStr = jni_str!("native_getSelectionStart");
const EDIT_TEXT_GET_SELECTION_END_NAME: &JNIStr = jni_str!("native_getSelectionEnd");
const EDIT_TEXT_GET_SELECTION_SIG: &JNIStr = jni_str!("(J)I");
const EDIT_TEXT_SET_SELECTION_NAME: &JNIStr = jni_str!("native_setSelection");
const EDIT_TEXT_SET_SELECTION_SIG: &JNIStr = jni_str!("(JI)V");

static ACTIVE_TEXT_FIELD: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
static ACTIVE_TEXT_SELECTION_ALL: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(0);
static ACTIVE_TEXT_CURSOR_UTF16: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(0);
static ACTIVE_TEXT_COMPOSING: std::sync::Mutex<Option<ComposingRegion>> =
    std::sync::Mutex::new(None);
static TEXTBOX_REFRESH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static HOST_IME_RESET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

const VIEW_VISIBLE: jint = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ComposingRegion {
    widget: i64,
    start_utf16: jint,
    end_utf16: jint,
}

const TEXT_POINTER_LIFETIME: std::time::Duration = std::time::Duration::from_millis(250);

#[derive(Clone, Copy)]
struct PendingTextPointer {
    position: (f32, f32),
    recorded_at: Instant,
}

static PENDING_TEXT_POINTER: std::sync::Mutex<Option<PendingTextPointer>> =
    std::sync::Mutex::new(None);

pub fn active_text_field() -> i64 {
    ACTIVE_TEXT_FIELD.load(std::sync::atomic::Ordering::Acquire)
}

pub fn clear_active_text_field() -> bool {
    let widget = ACTIVE_TEXT_FIELD.swap(0, std::sync::atomic::Ordering::AcqRel);
    reset_text_field_state();
    widget != 0
}

fn reset_text_field_state() {
    TEXTBOX_REFRESH.store(false, std::sync::atomic::Ordering::Release);
    ACTIVE_TEXT_SELECTION_ALL.store(0, std::sync::atomic::Ordering::Release);
    ACTIVE_TEXT_CURSOR_UTF16.store(0, std::sync::atomic::Ordering::Release);
    if let Ok(mut composing) = ACTIVE_TEXT_COMPOSING.lock() {
        discard_composing_region(&mut composing);
    }
    record_textbox_session(None);
}

fn discard_composing_region(composing: &mut Option<ComposingRegion>) {
    if composing.take().is_some() {
        HOST_IME_RESET.store(true, std::sync::atomic::Ordering::Release);
    }
}

fn composing_in(widget: i64) -> bool {
    ACTIVE_TEXT_COMPOSING
        .lock()
        .ok()
        .and_then(|composing| *composing)
        .is_some_and(|region| region.widget == widget)
}

pub(crate) fn take_host_ime_reset() -> bool {
    HOST_IME_RESET.swap(false, std::sync::atomic::Ordering::AcqRel)
}

fn request_textbox_refresh(widget: i64) {
    if widget != 0 && widget == active_text_field() {
        TEXTBOX_REFRESH.store(true, std::sync::atomic::Ordering::Release);
    }
}

fn text_field_visibility_changed(widget: i64, visibility: jint) {
    if visibility == VIEW_VISIBLE {
        request_textbox_refresh(widget);
    } else {
        clear_active_text_field_if(widget);
    }
}

pub fn select_all_active_text_field() -> bool {
    let widget = ACTIVE_TEXT_FIELD.load(std::sync::atomic::Ordering::Acquire);
    if widget == 0 {
        return false;
    }
    ACTIVE_TEXT_SELECTION_ALL.store(widget, std::sync::atomic::Ordering::Release);
    true
}

fn clear_active_text_field_if(widget: i64) {
    if widget != 0
        && ACTIVE_TEXT_FIELD
            .compare_exchange(
                widget,
                0,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    {
        reset_text_field_state();
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TextboxSession {
    widget: i64,
    geometry: (i32, i32, u32, u32),
    input_type: i32,
    font: i32,
    font_size: f32,
    multiline: bool,
    text_wrapped: bool,
    text_color: i32,
    x_alignment: i32,
    y_alignment: i32,
    editable: bool,
}

impl TextboxSession {
    fn accepts_input(&self) -> bool {
        self.geometry.2 > 0 && self.geometry.3 > 0
    }

    fn focus(&self) -> FocusedTextBox {
        FocusedTextBox {
            geometry: self.geometry,
            multiline: self.multiline,
            masked: text_input_type_masks_text(self.input_type),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FocusedTextBox {
    pub(crate) geometry: (i32, i32, u32, u32),
    pub(crate) multiline: bool,
    pub(crate) masked: bool,
}

pub(crate) fn text_input_type_masks_text(input_type: i32) -> bool {
    !matches!(input_type, 0..=4 | 6..=8 | 10)
}

pub(crate) fn displayed_text_box_text(text: &str, input_type: i32) -> std::borrow::Cow<'_, str> {
    if text_input_type_masks_text(input_type) {
        std::borrow::Cow::Owned("\u{2022}".repeat(text.chars().count()))
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

static TEXTBOX_SESSION: std::sync::Mutex<Option<TextboxSession>> = std::sync::Mutex::new(None);

pub fn textbox_geometry() -> Option<(i32, i32, u32, u32)> {
    TEXTBOX_SESSION
        .lock()
        .ok()
        .and_then(|session| session.map(|session| session.geometry))
}

pub fn textbox_input_type() -> i32 {
    TEXTBOX_SESSION
        .lock()
        .ok()
        .and_then(|session| session.map(|session| session.input_type))
        .unwrap_or(i32::MIN)
}

pub(crate) fn focused_text_box(vm: &Vm) -> Option<FocusedTextBox> {
    refreshed_text_box_session(vm).map(|session| session.focus())
}

fn live_text_box_session() -> Option<TextboxSession> {
    let active = ACTIVE_TEXT_FIELD.load(std::sync::atomic::Ordering::Acquire);
    TEXTBOX_SESSION
        .lock()
        .ok()
        .and_then(|session| *session)
        .filter(|session| {
            textbox_session_matches_active(*session, active) && session.accepts_input()
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextSelection {
    Cursor(usize),
    All,
}

pub(crate) struct ActiveTextOverlay {
    pub(crate) widget: i64,
    pub(crate) text: String,
    pub(crate) selection: TextSelection,
    pub(crate) geometry: (i32, i32, u32, u32),
    pub(crate) input_type: i32,
    pub(crate) font: i32,
    pub(crate) font_size: f32,
    pub(crate) multiline: bool,
    pub(crate) text_wrapped: bool,
    pub(crate) text_color: i32,
    pub(crate) x_alignment: i32,
    pub(crate) y_alignment: i32,
}

pub(crate) fn active_text_overlay() -> Option<ActiveTextOverlay> {
    let session = live_text_box_session()?;
    if (session.text_color as u32 >> 24) == 0
        || !(session.font_size.is_finite() && session.font_size > 0.0)
    {
        return None;
    }
    let (text, cursor) = view_registry::with_view(session.widget, |view| {
        (
            view.text.clone(),
            ACTIVE_TEXT_CURSOR_UTF16.load(std::sync::atomic::Ordering::Acquire),
        )
    })
    .ok()?;
    let text = text?;
    if !textbox_session_matches_active(
        session,
        ACTIVE_TEXT_FIELD.load(std::sync::atomic::Ordering::Acquire),
    ) {
        return None;
    }
    let selection =
        if ACTIVE_TEXT_SELECTION_ALL.load(std::sync::atomic::Ordering::Acquire) == session.widget {
            TextSelection::All
        } else {
            TextSelection::Cursor(usize::try_from(cursor).unwrap_or(0))
        };
    Some(ActiveTextOverlay {
        widget: session.widget,
        text,
        selection,
        geometry: session.geometry,
        input_type: session.input_type,
        font: session.font,
        font_size: session.font_size,
        multiline: session.multiline,
        text_wrapped: session.text_wrapped,
        text_color: session.text_color,
        x_alignment: session.x_alignment,
        y_alignment: session.y_alignment,
    })
}

fn textbox_session_matches_active(session: TextboxSession, active_widget: i64) -> bool {
    active_widget != 0 && session.widget == active_widget
}

fn has_live_textbox_session(widget: i64) -> bool {
    TEXTBOX_SESSION
        .lock()
        .ok()
        .and_then(|session| *session)
        .is_some_and(|session| textbox_session_matches_active(session, widget))
}

pub fn textbox_geometry_pending() -> bool {
    let widget = ACTIVE_TEXT_FIELD.load(std::sync::atomic::Ordering::Acquire);
    widget != 0
        && (TEXTBOX_REFRESH.load(std::sync::atomic::Ordering::Acquire)
            || !has_live_textbox_session(widget))
}

fn record_textbox_session(session: Option<TextboxSession>) {
    if let Ok(mut current) = TEXTBOX_SESSION.lock() {
        *current = session;
    }
    if let Some(session) = session {
        static LAST: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(i32::MIN);
        if LAST.swap(session.input_type, std::sync::atomic::Ordering::Relaxed) != session.input_type
        {
            tracing::info!(
                text_input_type = session.input_type,
                "focused textbox input type"
            );
        }
        let Some(pointer) = PENDING_TEXT_POINTER
            .lock()
            .ok()
            .and_then(|pointer| *pointer)
        else {
            return;
        };
        let expired = pointer.recorded_at.elapsed() > TEXT_POINTER_LIFETIME;
        if expired || update_active_text_cursor_from_pointer(session, pointer.position) {
            if let Ok(mut pending) = PENDING_TEXT_POINTER.lock() {
                if pending.is_some_and(|current| current.recorded_at == pointer.recorded_at) {
                    *pending = None;
                }
            }
        }
    }
}

static ACTIVE_TEXT_SCROLL: std::sync::Mutex<Option<(i64, crate::text_layout::Scroll)>> =
    std::sync::Mutex::new(None);

pub(crate) fn record_text_scroll(widget: i64, scroll: crate::text_layout::Scroll) {
    *ACTIVE_TEXT_SCROLL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((widget, scroll));
}

fn recorded_text_scroll(widget: i64) -> crate::text_layout::Scroll {
    ACTIVE_TEXT_SCROLL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .filter(|(scrolled, _)| *scrolled == widget)
        .map_or_else(crate::text_layout::Scroll::default, |(_, scroll)| scroll)
}

fn text_cursor_from_pointer(
    text: &str,
    cursor_utf16: usize,
    session: TextboxSession,
    position: (f32, f32),
) -> Option<jint> {
    let (x, y, width, height) = session.geometry;
    let relative_x = position.0 - x as f32;
    let relative_y = position.1 - y as f32;
    if relative_x < 0.0
        || relative_y < 0.0
        || relative_x > width as f32
        || relative_y > height as f32
    {
        return None;
    }
    let caret = crate::text_layout::char_index_at_utf16(text, cursor_utf16);
    let index = text_box_layout(text, caret, session)?.hit_test(relative_x, relative_y);
    jint::try_from(text.chars().take(index).map(char::len_utf16).sum::<usize>()).ok()
}

fn text_box_layout(
    text: &str,
    caret: usize,
    session: TextboxSession,
) -> Option<crate::text_layout::FieldLayout<'static>> {
    let (_, _, width, height) = session.geometry;
    let chain = roblox_fonts::face_chain(session.font)?;
    let displayed = displayed_text_box_text(text, session.input_type);
    let style = crate::text_layout::FieldStyle::of_text_box(
        session.font_size,
        (width, height),
        session.multiline,
        session.text_wrapped,
        session.x_alignment,
        session.y_alignment,
    );
    Some(crate::text_layout::lay_out(
        &displayed,
        crate::text_layout::Selection::Caret(caret),
        &style,
        chain,
        recorded_text_scroll(session.widget),
    ))
}

fn line_moved_cursor(
    session: TextboxSession,
    state: &TextFieldState,
    motion: crate::text_layout::LineMotion,
) -> Option<usize> {
    if state.select_all {
        return None;
    }
    let caret = state.text[..state.cursor].chars().count();
    let target = text_box_layout(&state.text, caret, session)?.line_motion(caret, motion);
    Some(
        state
            .text
            .char_indices()
            .nth(target)
            .map_or(state.text.len(), |(offset, _)| offset),
    )
}

fn line_motion(edit: TextEdit<'_>) -> Option<crate::text_layout::LineMotion> {
    let TextEdit::Move(motion) = edit else {
        return None;
    };
    match motion {
        TextMotion::Up => Some(crate::text_layout::LineMotion::Up),
        TextMotion::Down => Some(crate::text_layout::LineMotion::Down),
        TextMotion::LineStart => Some(crate::text_layout::LineMotion::Start),
        TextMotion::LineEnd => Some(crate::text_layout::LineMotion::End),
        TextMotion::Left(_)
        | TextMotion::Right(_)
        | TextMotion::TextStart
        | TextMotion::TextEnd => None,
    }
}

fn update_active_text_cursor_from_pointer(session: TextboxSession, position: (f32, f32)) -> bool {
    if active_text_field() != session.widget {
        return false;
    }
    let text =
        view_registry::with_view(session.widget, |view| view.text.clone().unwrap_or_default());
    let Ok(text) = text else {
        return false;
    };
    let current =
        usize::try_from(ACTIVE_TEXT_CURSOR_UTF16.load(std::sync::atomic::Ordering::Acquire))
            .unwrap_or(0);
    let Some(cursor) = text_cursor_from_pointer(&text, current, session, position) else {
        return false;
    };
    if !composing_in(session.widget) {
        ACTIVE_TEXT_CURSOR_UTF16.store(i64::from(cursor), std::sync::atomic::Ordering::Release);
    }
    true
}

pub(crate) fn prepare_text_field_pointer_press(position: (f32, f32)) -> bool {
    if let Ok(mut pending) = PENDING_TEXT_POINTER.lock() {
        *pending = Some(PendingTextPointer {
            position,
            recorded_at: Instant::now(),
        });
    }
    let widget = active_text_field();
    if widget == 0 {
        return false;
    }
    let session = TEXTBOX_SESSION
        .lock()
        .ok()
        .and_then(|session| *session)
        .filter(|session| textbox_session_matches_active(*session, widget));
    if let Some(session) = session {
        update_active_text_cursor_from_pointer(session, position);
    }
    ACTIVE_TEXT_SELECTION_ALL.store(0, std::sync::atomic::Ordering::Release);
    request_textbox_refresh(widget);
    true
}

pub fn query_textbox_geometry(vm: &Vm) {
    let raw = vm.as_raw();
    if raw.is_null() {
        return;
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    let widget = ACTIVE_TEXT_FIELD.load(std::sync::atomic::Ordering::Acquire);
    if widget == 0 {
        record_textbox_session(None);
        return;
    }
    let refresh = TEXTBOX_REFRESH.swap(false, std::sync::atomic::Ordering::AcqRel);
    if !refresh && has_live_textbox_session(widget) {
        return;
    }
    let _ = java_vm.attach_current_thread(|env: &mut Env| -> Result<(), FrameworkError> {
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let cls = match env.find_class(jni_str!("com/roblox/engine/jni/NativeGLInterface")) {
                Ok(c) => c,
                Err(_) => {
                    env.exception_clear();
                    return;
                }
            };
            let Ok(info) = checked(env, "NativeGLInterface.nativeGetTextBoxInfo", |env| {
                env.call_static_method(
                    &cls,
                    jni_str!("nativeGetTextBoxInfo"),
                    jni_sig!("()Lcom/roblox/engine/jni/model/NativeTextBoxInfo;"),
                    &[],
                )?
                .l()
            }) else {
                return;
            };
            if info.is_null() {
                clear_active_text_field_if(widget);
                return;
            }
            let float_sig = unsafe {
                FieldSignature::from_raw_parts(jni_str!("F"), JavaType::Primitive(Primitive::Float))
            };
            let int_sig = unsafe {
                FieldSignature::from_raw_parts(INT_SIG, JavaType::Primitive(Primitive::Int))
            };
            let bool_sig = unsafe {
                FieldSignature::from_raw_parts(
                    BOOLEAN_FIELD_SIG,
                    JavaType::Primitive(Primitive::Boolean),
                )
            };
            let read_float = |env: &mut Env, name: &JNIStr| -> Option<f32> {
                checked(env, "NativeTextBoxInfo float field", |env| {
                    env.get_field(&info, name, &float_sig)?.f()
                })
                .ok()
            };
            let read_int = |env: &mut Env, name: &JNIStr| -> Option<i32> {
                checked(env, "NativeTextBoxInfo int field", |env| {
                    env.get_field(&info, name, &int_sig)?.i()
                })
                .ok()
            };
            let read_bool = |env: &mut Env, name: &JNIStr| -> Option<bool> {
                checked(env, "NativeTextBoxInfo boolean field", |env| {
                    env.get_field(&info, name, &bool_sig)?.z()
                })
                .ok()
            };
            let (
                Some(x),
                Some(y),
                Some(w),
                Some(h),
                Some(font_size),
                Some(font),
                Some(input_type),
                Some(multiline),
                Some(text_wrapped),
                Some(text_color),
                Some(x_alignment),
                Some(y_alignment),
                Some(editable),
            ) = (
                read_float(env, jni_str!("x")),
                read_float(env, jni_str!("y")),
                read_float(env, jni_str!("width")),
                read_float(env, jni_str!("height")),
                read_float(env, jni_str!("fontSize")),
                read_int(env, jni_str!("font")),
                read_int(env, jni_str!("textInputType")),
                read_bool(env, jni_str!("multiline")),
                read_bool(env, jni_str!("textWrapped")),
                read_int(env, jni_str!("textColor")),
                read_int(env, jni_str!("xAlignment")),
                read_int(env, jni_str!("yAlignment")),
                read_bool(env, jni_str!("editable")),
            )
            else {
                record_textbox_session(None);
                return;
            };

            if ACTIVE_TEXT_FIELD.load(std::sync::atomic::Ordering::Acquire) == widget {
                record_textbox_session(Some(TextboxSession {
                    widget,
                    geometry: (x as i32, y as i32, w as u32, h as u32),
                    input_type,
                    font,
                    font_size,
                    multiline,
                    text_wrapped,
                    text_color,
                    x_alignment,
                    y_alignment,
                    editable,
                }));
            } else {
                record_textbox_session(None);
            }
        }));
        Ok(())
    });
}

extern "system" fn widget_native_set_text<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    text: JString<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let value = if text.is_null() {
            None
        } else {
            Some(text.try_to_string(env)?)
        };
        match record_widget_text(widget, value.clone()) {
            Ok(()) => {
                request_textbox_refresh(widget);
                tracing::debug!(
                target: "android.widget",
                widget,
                chars = value.as_deref().map_or(0, |text| text.chars().count()),
                "Widget.native_setText: recorded text length on non-GTK view peer"
                )
            }
            Err(e) => tracing::debug!(
                target: "android.widget",
                widget,
                error = %e,
                "Widget.native_setText: invalid view handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

fn record_widget_text(
    widget: i64,
    value: Option<String>,
) -> Result<(), view_registry::ViewRegistryError> {
    view_registry::with_view(widget, |view| {
        if view.text != value && active_text_field() == widget {
            let text = value.as_deref().unwrap_or_default();
            ACTIVE_TEXT_CURSOR_UTF16.store(
                i64::from(java_cursor_position(text)),
                std::sync::atomic::Ordering::Release,
            );
            ACTIVE_TEXT_SELECTION_ALL.store(0, std::sync::atomic::Ordering::Release);
            let previous = view.text.as_deref().unwrap_or_default();
            if let Ok(mut composing) = ACTIVE_TEXT_COMPOSING.lock() {
                let preserved = composing.is_some_and(|region| {
                    region.widget == widget
                        && composed_text(previous, region)
                            .is_some_and(|old| composed_text(text, region) == Some(old))
                });
                if !preserved {
                    discard_composing_region(&mut composing);
                }
            }
        }
        view.text = value;
    })
}

fn composed_text(text: &str, region: ComposingRegion) -> Option<&str> {
    utf16_range_byte_offsets(text, region.start_utf16, region.end_utf16).map(|range| &text[range])
}

extern "system" fn edit_text_native_get_text<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        let previous = ACTIVE_TEXT_FIELD.swap(widget, std::sync::atomic::Ordering::AcqRel);
        let text = view_registry::with_view(widget, |v| v.text.clone())
            .ok()
            .flatten()
            .unwrap_or_default();
        let text_end = i64::from(java_cursor_position(&text));
        if previous != widget {
            ACTIVE_TEXT_CURSOR_UTF16.store(text_end, std::sync::atomic::Ordering::Release);
        } else {
            ACTIVE_TEXT_CURSOR_UTF16.fetch_min(text_end, std::sync::atomic::Ordering::AcqRel);
        }
        env.new_string(&text)
    })
    .resolve::<LogErrorAndDefault>()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectionEdge {
    Start,
    End,
}

fn text_field_selection(
    widget: i64,
    edge: SelectionEdge,
) -> Result<jint, view_registry::ViewRegistryError> {
    view_registry::with_view(widget, |view| {
        let length = java_cursor_position(view.text.as_deref().unwrap_or_default());
        if ACTIVE_TEXT_SELECTION_ALL.load(std::sync::atomic::Ordering::Acquire) == widget {
            return match edge {
                SelectionEdge::Start => 0,
                SelectionEdge::End => length,
            };
        }
        if active_text_field() != widget {
            return length;
        }
        active_cursor_utf16().min(length)
    })
}

fn set_text_field_selection(
    widget: i64,
    index: jint,
) -> Result<(), view_registry::ViewRegistryError> {
    view_registry::with_view(widget, |view| {
        if active_text_field() != widget {
            return;
        }
        let (_, cursor) = utf16_cursor_byte_offset(view.text.as_deref().unwrap_or_default(), index);
        ACTIVE_TEXT_CURSOR_UTF16.store(i64::from(cursor), std::sync::atomic::Ordering::Release);
        ACTIVE_TEXT_SELECTION_ALL.store(0, std::sync::atomic::Ordering::Release);
    })
}

fn edit_text_selection(mut env: EnvUnowned<'_>, widget: jlong, edge: SelectionEdge) -> jint {
    env.with_env(|_env| -> jni::errors::Result<jint> {
        Ok(text_field_selection(widget, edge).unwrap_or_else(|e| {
            tracing::debug!(
                target: "android.widget.EditText",
                widget,
                ?edge,
                error = %e,
                "EditText selection: invalid view handle (reported 0)"
            );
            0
        }))
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn edit_text_native_get_selection_start<'local>(
    env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
) -> jint {
    edit_text_selection(env, widget, SelectionEdge::Start)
}

extern "system" fn edit_text_native_get_selection_end<'local>(
    env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
) -> jint {
    edit_text_selection(env, widget, SelectionEdge::End)
}

extern "system" fn edit_text_native_set_selection<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    index: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = set_text_field_selection(widget, index) {
            tracing::debug!(
                target: "android.widget.EditText",
                widget,
                error = %e,
                "EditText.setSelection: invalid view handle (ignored)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

fn utf16_cursor_byte_offset(text: &str, requested: jint) -> (usize, jint) {
    let requested = requested.max(0) as usize;
    let mut utf16_offset = 0usize;
    for (byte_offset, character) in text.char_indices() {
        let next_utf16 = utf16_offset.saturating_add(character.len_utf16());
        if next_utf16 > requested {
            return (
                byte_offset,
                jint::try_from(utf16_offset).unwrap_or(jint::MAX),
            );
        }
        utf16_offset = next_utf16;
        if utf16_offset == requested {
            return (
                byte_offset + character.len_utf8(),
                jint::try_from(utf16_offset).unwrap_or(jint::MAX),
            );
        }
    }
    (
        text.len(),
        jint::try_from(utf16_offset).unwrap_or(jint::MAX),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextUnit {
    Character,
    Word,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextMotion {
    Left(TextUnit),
    Right(TextUnit),
    Up,
    Down,
    LineStart,
    LineEnd,
    TextStart,
    TextEnd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextEdit<'a> {
    Type(&'a str),
    Paste(&'a str),
    Backspace(TextUnit),
    ForwardDelete(TextUnit),
    Move(TextMotion),
    Compose {
        text: &'a str,
        cursor: Option<usize>,
    },
    FinishComposing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaretStep {
    Grapheme,
    Char,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TextFieldRules {
    multiline: bool,
    editable: bool,
    caret_step: CaretStep,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TextFieldState {
    text: String,
    cursor: usize,
    select_all: bool,
    composing: Option<std::ops::Range<usize>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CharClass {
    Space,
    Word,
    Punctuation,
}

fn char_class(character: char) -> CharClass {
    if character.is_whitespace() {
        CharClass::Space
    } else if character.is_alphanumeric() || character == '_' {
        CharClass::Word
    } else {
        CharClass::Punctuation
    }
}

fn grapheme_class(grapheme: &str) -> CharClass {
    grapheme.chars().next().map_or(CharClass::Space, char_class)
}

fn skip_word<'a>(
    graphemes: &mut std::iter::Peekable<impl Iterator<Item = (usize, &'a str)>>,
    boundary: impl Fn(usize, &str) -> usize,
    mut edge: usize,
) -> usize {
    while let Some(&(offset, grapheme)) = graphemes.peek() {
        if grapheme_class(grapheme) != CharClass::Space {
            break;
        }
        edge = boundary(offset, grapheme);
        graphemes.next();
    }
    let Some(&(_, first)) = graphemes.peek() else {
        return edge;
    };
    let class = grapheme_class(first);
    while let Some(&(offset, grapheme)) = graphemes.peek() {
        if grapheme_class(grapheme) != class {
            break;
        }
        edge = boundary(offset, grapheme);
        graphemes.next();
    }
    edge
}

fn word_start_before(text: &str, cursor: usize) -> usize {
    let mut graphemes = text[..cursor].grapheme_indices(true).rev().peekable();
    skip_word(&mut graphemes, |offset, _| offset, cursor)
}

fn word_end_after(text: &str, cursor: usize) -> usize {
    let mut graphemes = text[cursor..]
        .grapheme_indices(true)
        .map(|(offset, grapheme)| (cursor + offset, grapheme))
        .peekable();
    skip_word(
        &mut graphemes,
        |offset, grapheme| offset + grapheme.len(),
        cursor,
    )
}

fn previous_boundary(text: &str, cursor: usize, step: CaretStep) -> usize {
    let before = &text[..cursor];
    let length = match step {
        CaretStep::Grapheme => before.graphemes(true).next_back().map_or(0, str::len),
        CaretStep::Char => before.chars().next_back().map_or(0, char::len_utf8),
    };
    cursor - length
}

fn next_boundary(text: &str, cursor: usize, step: CaretStep) -> usize {
    let after = &text[cursor..];
    let length = match step {
        CaretStep::Grapheme => after.graphemes(true).next().map_or(0, str::len),
        CaretStep::Char => after.chars().next().map_or(0, char::len_utf8),
    };
    cursor + length
}

fn joins_an_emoji_sequence(character: char) -> bool {
    use unicode_properties::emoji::{is_regional_indicator, is_tag_character, is_zwj};
    is_zwj(character)
        || is_regional_indicator(character)
        || is_tag_character(character)
        || matches!(
            character,
            '\u{20E3}'
                | '\u{FE00}'..='\u{FE0F}'
                | '\u{1F3FB}'..='\u{1F3FF}'
                | '\u{E0100}'..='\u{E01EF}'
        )
}

fn backspace_boundary(text: &str, cursor: usize) -> usize {
    match text[..cursor].grapheme_indices(true).next_back() {
        Some((start, grapheme)) if grapheme.chars().any(joins_an_emoji_sequence) => start,
        _ => previous_boundary(text, cursor, CaretStep::Char),
    }
}

fn line_start(text: &str, cursor: usize) -> usize {
    text[..cursor].rfind('\n').map_or(0, |newline| newline + 1)
}

fn line_end(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .find('\n')
        .map_or(text.len(), |newline| cursor + newline)
}

fn column_in_line(text: &str, start: usize, column: usize) -> usize {
    let end = line_end(text, start);
    text[start..end]
        .grapheme_indices(true)
        .nth(column)
        .map_or(end, |(offset, _)| start + offset)
}

fn column(text: &str, cursor: usize) -> usize {
    text[line_start(text, cursor)..cursor]
        .graphemes(true)
        .count()
}

fn line_above(text: &str, cursor: usize) -> usize {
    match line_start(text, cursor) {
        0 => 0,
        start => column_in_line(text, line_start(text, start - 1), column(text, cursor)),
    }
}

fn line_below(text: &str, cursor: usize) -> usize {
    match text[cursor..].find('\n') {
        Some(newline) => column_in_line(text, cursor + newline + 1, column(text, cursor)),
        None => text.len(),
    }
}

fn moved_cursor(text: &str, cursor: usize, motion: TextMotion, step: CaretStep) -> usize {
    match motion {
        TextMotion::Left(TextUnit::Character) => previous_boundary(text, cursor, step),
        TextMotion::Right(TextUnit::Character) => next_boundary(text, cursor, step),
        TextMotion::Left(TextUnit::Word) => word_start_before(text, cursor),
        TextMotion::Right(TextUnit::Word) => word_end_after(text, cursor),
        TextMotion::Up => line_above(text, cursor),
        TextMotion::Down => line_below(text, cursor),
        TextMotion::LineStart => line_start(text, cursor),
        TextMotion::LineEnd => line_end(text, cursor),
        TextMotion::TextStart => 0,
        TextMotion::TextEnd => text.len(),
    }
}

fn collapses_to_start(motion: TextMotion) -> bool {
    matches!(
        motion,
        TextMotion::Left(_) | TextMotion::Up | TextMotion::LineStart | TextMotion::TextStart
    )
}

const ROBLOX_TEXT_BOX_MAX_UTF16: usize = 64_000;

fn insertable_text(text: &str, multiline: bool) -> String {
    text.replace("\r\n", "\n")
        .chars()
        .map(|character| if character == '\r' { '\n' } else { character })
        .filter(|character| {
            !character.is_control() || (multiline && matches!(character, '\n' | '\t'))
        })
        .collect()
}

fn within_text_box_limit(inserted: &str, kept_utf16: usize) -> &str {
    let mut room = ROBLOX_TEXT_BOX_MAX_UTF16.saturating_sub(kept_utf16);
    let end = inserted
        .char_indices()
        .find(
            |&(_, character)| match room.checked_sub(character.len_utf16()) {
                Some(left) => {
                    room = left;
                    false
                }
                None => true,
            },
        )
        .map_or(inserted.len(), |(offset, _)| offset);
    &inserted[..end]
}

fn insert_text(state: TextFieldState, inserted: &str) -> TextFieldState {
    let kept_utf16 = if state.select_all {
        0
    } else {
        state.text.encode_utf16().count()
    };
    let inserted = within_text_box_limit(inserted, kept_utf16);
    if inserted.is_empty() {
        return state;
    }
    let (mut text, at) = if state.select_all {
        (String::new(), 0)
    } else {
        (state.text, state.cursor)
    };
    text.insert_str(at, inserted);
    TextFieldState {
        text,
        cursor: at + inserted.len(),
        select_all: false,
        composing: None,
    }
}

fn delete_toward(state: TextFieldState, motion: TextMotion) -> TextFieldState {
    if state.select_all {
        return TextFieldState {
            text: String::new(),
            cursor: 0,
            select_all: false,
            composing: None,
        };
    }
    let edge = match motion {
        TextMotion::Left(TextUnit::Character) => backspace_boundary(&state.text, state.cursor),
        motion => moved_cursor(&state.text, state.cursor, motion, CaretStep::Grapheme),
    };
    let range = state.cursor.min(edge)..state.cursor.max(edge);
    let mut text = state.text;
    text.replace_range(range.clone(), "");
    TextFieldState {
        text,
        cursor: range.start,
        select_all: false,
        composing: None,
    }
}

fn compose_text(
    state: TextFieldState,
    preedit: &str,
    preedit_cursor: Option<usize>,
    multiline: bool,
) -> TextFieldState {
    if preedit.is_empty() && state.composing.is_none() {
        return state;
    }
    let (mut text, at) = match (state.select_all, state.composing) {
        (true, _) => (String::new(), 0),
        (false, Some(region)) => {
            let mut text = state.text;
            text.replace_range(region.clone(), "");
            (text, region.start)
        }
        (false, None) => (state.text, state.cursor),
    };
    let insertable = insertable_text(preedit, multiline);
    let inserted = within_text_box_limit(&insertable, text.encode_utf16().count());
    text.insert_str(at, inserted);
    let caret = preedit_cursor
        .filter(|&caret| inserted == preedit && inserted.is_char_boundary(caret))
        .unwrap_or(inserted.len());
    TextFieldState {
        text,
        cursor: at + caret,
        select_all: false,
        composing: (!inserted.is_empty()).then_some(at..at + inserted.len()),
    }
}

fn apply_text_edit(
    state: TextFieldState,
    edit: TextEdit<'_>,
    rules: TextFieldRules,
) -> TextFieldState {
    if let TextEdit::Compose { text, cursor } = edit {
        return compose_text(state, text, cursor, rules.multiline);
    }
    let state = TextFieldState {
        composing: None,
        ..state
    };
    match edit {
        TextEdit::Type(typed) => insert_text(state, &insertable_text(typed, rules.multiline)),
        TextEdit::Paste(_) if !rules.editable => state,
        TextEdit::Paste(pasted) => insert_text(state, &insertable_text(pasted, rules.multiline)),
        TextEdit::Backspace(unit) => delete_toward(state, TextMotion::Left(unit)),
        TextEdit::ForwardDelete(unit) => delete_toward(state, TextMotion::Right(unit)),
        TextEdit::Move(motion) => {
            let cursor = match (state.select_all, collapses_to_start(motion)) {
                (true, true) => 0,
                (true, false) => state.text.len(),
                (false, _) => moved_cursor(&state.text, state.cursor, motion, rules.caret_step),
            };
            TextFieldState {
                cursor,
                select_all: false,
                ..state
            }
        }
        TextEdit::Compose { .. } | TextEdit::FinishComposing => state,
    }
}

fn selected_text(state: &TextFieldState, masked: bool) -> Option<&str> {
    (state.select_all && !masked && !state.text.is_empty()).then_some(state.text.as_str())
}

fn java_cursor_position(text: &str) -> jint {
    jint::try_from(text.encode_utf16().count()).unwrap_or(jint::MAX)
}

fn utf16_range_byte_offsets(text: &str, start: jint, end: jint) -> Option<std::ops::Range<usize>> {
    let (start_byte, start_utf16) = utf16_cursor_byte_offset(text, start);
    let (end_byte, end_utf16) = utf16_cursor_byte_offset(text, end);
    (start_utf16 == start && end_utf16 == end && start_byte <= end_byte)
        .then_some(start_byte..end_byte)
}

fn active_cursor_utf16() -> jint {
    ACTIVE_TEXT_CURSOR_UTF16
        .load(std::sync::atomic::Ordering::Acquire)
        .clamp(0, i64::from(jint::MAX)) as jint
}

fn load_text_field_state(widget: i64, text: String) -> TextFieldState {
    let (cursor, _) = utf16_cursor_byte_offset(&text, active_cursor_utf16());
    let composing = ACTIVE_TEXT_COMPOSING
        .lock()
        .ok()
        .and_then(|composing| *composing)
        .filter(|region| region.widget == widget)
        .and_then(|region| utf16_range_byte_offsets(&text, region.start_utf16, region.end_utf16));
    TextFieldState {
        select_all: ACTIVE_TEXT_SELECTION_ALL.load(std::sync::atomic::Ordering::Acquire) == widget,
        text,
        cursor,
        composing,
    }
}

fn store_text_field_state(widget: i64, state: &TextFieldState) -> jint {
    let cursor = java_cursor_position(&state.text[..state.cursor]);
    ACTIVE_TEXT_CURSOR_UTF16.store(i64::from(cursor), std::sync::atomic::Ordering::Release);
    ACTIVE_TEXT_SELECTION_ALL.store(
        if state.select_all { widget } else { 0 },
        std::sync::atomic::Ordering::Release,
    );
    if let Ok(mut composing) = ACTIVE_TEXT_COMPOSING.lock() {
        *composing = state.composing.as_ref().map(|region| ComposingRegion {
            widget,
            start_utf16: java_cursor_position(&state.text[..region.start]),
            end_utf16: java_cursor_position(&state.text[..region.end]),
        });
    }
    cursor
}

fn refreshed_text_box_session(vm: &Vm) -> Option<TextboxSession> {
    if textbox_geometry_pending() {
        query_textbox_geometry(vm);
    }
    live_text_box_session()
}

pub(crate) fn edit_active_text_field(vm: &Vm, edit: TextEdit<'_>) -> bool {
    let Some(session) = refreshed_text_box_session(vm) else {
        return false;
    };
    match edit_text_box(session, edit) {
        Ok((text, cursor, changed)) => {
            if changed {
                sync_engine_textbox(vm, &text, cursor);
            }
            true
        }
        Err(_) => {
            clear_active_text_field_if(session.widget);
            false
        }
    }
}

fn edit_text_box(
    session: TextboxSession,
    edit: TextEdit<'_>,
) -> Result<(String, jint, bool), view_registry::ViewRegistryError> {
    let widget = session.widget;
    let rules = TextFieldRules {
        multiline: session.multiline,
        editable: session.editable,
        caret_step: if text_input_type_masks_text(session.input_type) {
            CaretStep::Char
        } else {
            CaretStep::Grapheme
        },
    };
    let line_move = line_motion(edit).and_then(|motion| {
        let text =
            view_registry::with_view(widget, |view| view.text.clone().unwrap_or_default()).ok()?;
        let from = load_text_field_state(widget, text);
        let to = line_moved_cursor(session, &from, motion)?;
        Some((from, to))
    });
    view_registry::with_view(widget, |view| {
        let before = load_text_field_state(widget, view.text.clone().unwrap_or_default());
        let after = match line_move {
            Some((from, to)) if from == before => TextFieldState {
                cursor: to,
                select_all: false,
                composing: None,
                ..before.clone()
            },
            _ => apply_text_edit(before.clone(), edit, rules),
        };
        let cursor = store_text_field_state(widget, &after);
        view.text = Some(after.text.clone());
        let changed = before != after;
        (after.text, cursor, changed)
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SelectionTransfer {
    Copy,
    Cut,
}

pub(crate) fn active_text_field_selection(vm: &Vm, transfer: SelectionTransfer) -> Option<String> {
    let session = refreshed_text_box_session(vm)?;
    if transfer == SelectionTransfer::Cut && !session.editable {
        return None;
    }
    let widget = session.widget;
    let text =
        view_registry::with_view(widget, |view| view.text.clone().unwrap_or_default()).ok()?;
    let state = load_text_field_state(widget, text);
    selected_text(&state, text_input_type_masks_text(session.input_type)).map(str::to_owned)
}

const KEYCODE_BACK: jint = 4;
const KEYCODE_ENTER: jint = 66;
const EDITOR_INFO_IME_NULL: jint = 0;

pub(crate) fn submit_active_text_field(vm: &Vm) -> Result<(), FrameworkError> {
    let Some(session) = refreshed_text_box_session(vm) else {
        return Ok(());
    };
    let widget = session.widget;
    with_attached_env(vm, |env| run_editor_action(env, widget))?;
    if active_text_field() != widget {
        return Ok(());
    }
    let text = view_registry::with_view(widget, |view| view.text.clone().unwrap_or_default())
        .map_err(FrameworkError::ViewRegistry)?;
    let (_, cursor) = utf16_cursor_byte_offset(&text, active_cursor_utf16());
    sync_engine_textbox(vm, &text, cursor);
    Ok(())
}

pub(crate) fn release_active_text_field(vm: &Vm) -> Result<(), FrameworkError> {
    let widget = active_text_field();
    if widget == 0 {
        return Ok(());
    }
    with_attached_env(vm, |env| release_back_on_text_field(env, widget))?;
    clear_active_text_field_if(widget);
    Ok(())
}

fn release_back_on_text_field(env: &mut Env, widget: i64) -> Result<bool, FrameworkError> {
    let view = text_field_view(env, widget)?;
    let back = key_event(env, KeyAction::Up, KEYCODE_BACK)?;
    checked(env, "RbxKeyboard.onKeyPreIme(BACK)", |env| {
        env.call_method(
            &view,
            jni_str!("onKeyPreIme"),
            jni_sig!("(ILandroid/view/KeyEvent;)Z"),
            &[JValue::Int(KEYCODE_BACK), JValue::Object(&back)],
        )?
        .z()
    })
}

fn with_attached_env<T>(
    vm: &Vm,
    operation: impl FnOnce(&mut Env) -> Result<T, FrameworkError>,
) -> Result<T, FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }
    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| operation(env))) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

fn text_field_view<'local>(
    env: &mut Env<'local>,
    widget: i64,
) -> Result<JObject<'local>, FrameworkError> {
    view_registry::local_jobject(env, widget)
        .map_err(FrameworkError::ViewRegistry)??
        .ok_or(FrameworkError::Jni(jni::errors::Error::NullPtr(
            "text field view has no live Java peer",
        )))
}

fn key_event<'local>(
    env: &mut Env<'local>,
    action: KeyAction,
    key_code: jint,
) -> Result<JObject<'local>, FrameworkError> {
    let class = checked(env, "KeyEvent class", |env| env.find_class(KEY_EVENT_CLASS))?;
    checked(env, "KeyEvent(action, code)", |env| {
        env.new_object(
            &class,
            jni_sig!("(II)V"),
            &[JValue::Int(action.code()), JValue::Int(key_code)],
        )
    })
}

fn run_editor_action(env: &mut Env, widget: i64) -> Result<(), FrameworkError> {
    let view = text_field_view(env, widget)?;
    let listener = view_registry::with_view(widget, |view| {
        view.editor_action_listener
            .as_ref()
            .map(|listener| env.new_local_ref(listener.as_obj()))
    })
    .map_err(FrameworkError::ViewRegistry)?
    .transpose()?;
    let Some(listener) = listener else {
        tracing::debug!(
            widget,
            "Enter in a single-line text field: no editor-action listener to run"
        );
        return Ok(());
    };
    let enter = key_event(env, KeyAction::Down, KEYCODE_ENTER)?;
    checked(
        env,
        "TextView.OnEditorActionListener.onEditorAction",
        |env| {
            env.call_method(
                &listener,
                jni_str!("onEditorAction"),
                jni_sig!("(Landroid/widget/TextView;ILandroid/view/KeyEvent;)Z"),
                &[
                    JValue::Object(&view),
                    JValue::Int(EDITOR_INFO_IME_NULL),
                    JValue::Object(&enter),
                ],
            )?
            .z()
        },
    )?;
    Ok(())
}

fn sync_engine_textbox(vm: &Vm, text: &str, cursor: jint) {
    let raw = vm.as_raw();
    if raw.is_null() {
        return;
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    let _ = java_vm.attach_current_thread(|env: &mut Env| -> Result<(), FrameworkError> {
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let cls = match env.find_class(jni_str!("com/roblox/engine/jni/NativeGLInterface")) {
                Ok(c) => c,
                Err(_) => {
                    env.exception_clear();
                    return;
                }
            };
            let Ok(s) = env.new_string(text) else {
                return;
            };
            if let Err(e) = checked(
                env,
                "NativeGLInterface.syncTextboxTextAndCursorPosition2",
                |env| {
                    env.call_static_method(
                        &cls,
                        jni_str!("syncTextboxTextAndCursorPosition2"),
                        jni_sig!("(Ljava/lang/String;I)V"),
                        &[JValue::Object(&s), JValue::Int(cursor)],
                    )?
                    .v()
                },
            ) {
                tracing::debug!(error = %e, "syncTextboxTextAndCursorPosition2 threw (cleared)");
            }
        }));
        Ok(())
    });
}

pub fn reflect_engine_input_methods(vm: &Vm) {
    let raw = vm.as_raw();
    if raw.is_null() {
        return;
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    let _ = java_vm.attach_current_thread(|env: &mut Env| -> Result<(), FrameworkError> {
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            for class_name in [
                jni_str!("com/roblox/engine/jni/NativeGLInterface"),
                jni_str!("com/roblox/engine/jni/NativeInputInterface"),
            ] {
                let cls = match env.find_class(class_name) {
                    Ok(c) => c,
                    Err(_) => {
                        env.exception_clear();
                        tracing::info!(class = %class_name.to_str(), "reflect-input: class not loaded");
                        continue;
                    }
                };
                let methods_obj = match checked(env, "Class.getDeclaredMethods", |env| {
                    env.call_method(
                        &cls,
                        jni_str!("getDeclaredMethods"),
                        jni_sig!("()[Ljava/lang/reflect/Method;"),
                        &[],
                    )?
                    .l()
                }) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let methods: JObjectArray = match env.cast_local::<JObjectArray>(methods_obj) {
                    Ok(a) => a,
                    Err(_) => continue,
                };
                let len = methods.len(env).unwrap_or(0);
                tracing::info!(class = %class_name.to_str(), methods = len, "reflect-input: declared methods");
                for i in 0..len {
                    let Ok(m) = methods.get_element(env, i) else {
                        continue;
                    };
                    let Ok(s_obj) = checked(env, "Method.toString", |env| {
                        env.call_method(
                            &m,
                            jni_str!("toString"),
                            jni_sig!("()Ljava/lang/String;"),
                            &[],
                        )?
                        .l()
                    }) else {
                        continue;
                    };

                    let s = unsafe { JString::from_raw(env, s_obj.into_raw()) };
                    if let Ok(desc) = s.try_to_string(env) {
                        if desc.contains("Pass")
                            || desc.contains("Input")
                            || desc.contains("Text")
                            || desc.contains("Key")
                            || desc.contains("Char")
                        {
                            tracing::info!(method = %desc, "reflect-input: bridge method");
                        }
                    }
                }
            }
        }));
        Ok(())
    });
}

extern "system" fn radio_button_set_text<'local>(
    mut env: EnvUnowned<'local>,
    this: JObject<'local>,
    text: JObject<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let widget = view_widget_handle(env, &this);

        let value = if text.is_null() {
            None
        } else {
            let s = env
                .call_method(
                    &text,
                    jni_str!("toString"),
                    jni_sig!("()Ljava/lang/String;"),
                    &[],
                )?
                .l()?;
            Some(JString::cast_local(env, s)?.try_to_string(env)?)
        };
        match view_registry::with_view(widget, |v| v.text = value.clone()) {
            Ok(()) => tracing::debug!(
                target: "android.widget.RadioButton",
                widget,
                chars = value.as_deref().map_or(0, |text| text.chars().count()),
                "RadioButton.setText: recorded text length on non-GTK view peer"
            ),
            Err(e) => tracing::debug!(
                target: "android.widget.RadioButton",
                widget,
                error = %e,
                "RadioButton.setText: invalid view handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn progress_bar_set_indeterminate<'local>(
    mut env: EnvUnowned<'local>,
    this: JObject<'local>,
    indeterminate: jboolean,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let widget = view_widget_handle(env, &this);
        if let Err(e) = view_registry::with_view(widget, |_v| ()) {
            tracing::debug!(
                target: "android.widget.ProgressBar",
                widget,
                indeterminate,
                error = %e,
                "ProgressBar.native_setIndeterminate: invalid view handle (ignored)"
            );
        } else {
            tracing::trace!(
                target: "android.widget.ProgressBar",
                widget,
                indeterminate,
                "ProgressBar.native_setIndeterminate: validated handle, no-op (no progress chrome drawn)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn progress_native_set_progress<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    fraction: jfloat,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        match view_registry::with_view(widget, |_v| ()) {
            Ok(()) => tracing::trace!(
                target: "android.widget",
                widget,
                fraction,
                "Widget.native_setProgress: validated handle, no-op (no progress chrome drawn)"
            ),
            Err(e) => tracing::debug!(
                target: "android.widget",
                widget,
                error = %e,
                "Widget.native_setProgress: invalid view handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn seek_bar_set_max<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    max: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        match view_registry::with_view(widget, |_v| ()) {
            Ok(()) => tracing::trace!(
                target: "android.widget.SeekBar",
                widget,
                max,
                "SeekBar.native_setMax: validated handle, no-op (no seek-bar chrome drawn)"
            ),
            Err(e) => tracing::debug!(
                target: "android.widget.SeekBar",
                widget,
                error = %e,
                "SeekBar.native_setMax: invalid view handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn button_set_compound_drawables<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    paintable: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        match view_registry::with_view(widget, |_v| ()) {
            Ok(()) => tracing::trace!(
                target: "android.widget.Button",
                widget,
                paintable,
                "Button.native_setCompoundDrawables: validated handle, no-op (drawable draw deferred)"
            ),
            Err(e) => tracing::debug!(
                target: "android.widget.Button",
                widget,
                error = %e,
                "Button.native_setCompoundDrawables: invalid view handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn spinner_set_adapter<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    _adapter: JObject<'local>,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        match view_registry::with_view(widget, |_v| ()) {
            Ok(()) => tracing::trace!(
                target: "android.widget.Spinner",
                widget,
                "Spinner.native_setAdapter: validated handle, no-op (no spinner dropdown drawn)"
            ),
            Err(e) => tracing::debug!(
                target: "android.widget.Spinner",
                widget,
                error = %e,
                "Spinner.native_setAdapter: invalid view handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn edit_text_add_text_changed_listener<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    watcher: JObject<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if watcher.is_null() {
            tracing::debug!(
                target: "android.widget.EditText",
                widget,
                "EditText.native_addTextChangedListener: null watcher (ignored)"
            );
            return Ok(());
        }

        let global = env.new_global_ref(&watcher)?;
        match view_registry::add_text_watcher(widget, global) {
            Ok(()) => tracing::debug!(
                target: "android.widget.EditText",
                widget,
                "EditText.native_addTextChangedListener: retained TextWatcher on non-GTK view peer (dispatch on real input is a future step)"
            ),
            Err(e) => tracing::debug!(
                target: "android.widget.EditText",
                widget,
                error = %e,
                "EditText.native_addTextChangedListener: invalid view handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn edit_text_remove_text_changed_listener<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    watcher: JObject<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if watcher.is_null() {
            tracing::debug!(
                target: "android.widget.EditText",
                widget,
                "EditText.native_removeTextChangedListener: null watcher (ignored)"
            );
            return Ok(());
        }

        let result = view_registry::retain_text_watchers(widget, |held| {
            !env.is_same_object(held.as_obj(), &watcher).unwrap_or(false)
        });
        match result {
            Ok(dropped) => tracing::debug!(
                target: "android.widget.EditText",
                widget,
                dropped,
                "EditText.native_removeTextChangedListener: dropped matching retained TextWatcher(s)"
            ),
            Err(e) => tracing::debug!(
                target: "android.widget.EditText",
                widget,
                error = %e,
                "EditText.native_removeTextChangedListener: invalid view handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn edit_text_set_on_editor_action_listener<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    listener: JObject<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let global = if listener.is_null() {
            None
        } else {
            Some(env.new_global_ref(&listener)?)
        };
        match view_registry::set_editor_action_listener(widget, global) {
            Ok(()) => tracing::debug!(
                target: "android.widget.EditText",
                widget,
                cleared = listener.is_null(),
                "EditText.native_setOnEditorActionListener: retained editor-action listener on non-GTK view peer (dispatch on real input is a future step)"
            ),
            Err(e) => tracing::debug!(
                target: "android.widget.EditText",
                widget,
                error = %e,
                "EditText.native_setOnEditorActionListener: invalid view handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

fn register_widget_property_setter_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let button: [NativeBinding; 2] = [
        (
            WIDGET_NATIVE_SET_TEXT_NAME,
            WIDGET_NATIVE_SET_TEXT_SIG,
            widget_native_set_text as *mut c_void,
        ),
        (
            BUTTON_SET_COMPOUND_DRAWABLES_NAME,
            BUTTON_SET_COMPOUND_DRAWABLES_SIG,
            button_set_compound_drawables as *mut c_void,
        ),
    ];
    register_class_natives_best_effort(env, BUTTON_CLASS, &button)?;

    let edit_text: [NativeBinding; 8] = [
        (
            WIDGET_NATIVE_SET_TEXT_NAME,
            WIDGET_NATIVE_SET_TEXT_SIG,
            widget_native_set_text as *mut c_void,
        ),
        (
            EDIT_TEXT_GET_TEXT_NAME,
            EDIT_TEXT_GET_TEXT_SIG,
            edit_text_native_get_text as *mut c_void,
        ),
        (
            EDIT_TEXT_GET_SELECTION_START_NAME,
            EDIT_TEXT_GET_SELECTION_SIG,
            edit_text_native_get_selection_start as *mut c_void,
        ),
        (
            EDIT_TEXT_GET_SELECTION_END_NAME,
            EDIT_TEXT_GET_SELECTION_SIG,
            edit_text_native_get_selection_end as *mut c_void,
        ),
        (
            EDIT_TEXT_SET_SELECTION_NAME,
            EDIT_TEXT_SET_SELECTION_SIG,
            edit_text_native_set_selection as *mut c_void,
        ),
        (
            EDIT_TEXT_ADD_TEXT_CHANGED_LISTENER_NAME,
            EDIT_TEXT_TEXT_CHANGED_LISTENER_SIG,
            edit_text_add_text_changed_listener as *mut c_void,
        ),
        (
            EDIT_TEXT_REMOVE_TEXT_CHANGED_LISTENER_NAME,
            EDIT_TEXT_TEXT_CHANGED_LISTENER_SIG,
            edit_text_remove_text_changed_listener as *mut c_void,
        ),
        (
            EDIT_TEXT_SET_ON_EDITOR_ACTION_LISTENER_NAME,
            EDIT_TEXT_SET_ON_EDITOR_ACTION_LISTENER_SIG,
            edit_text_set_on_editor_action_listener as *mut c_void,
        ),
    ];
    register_class_natives_best_effort(env, EDIT_TEXT_CLASS, &edit_text)?;

    let check_box: [NativeBinding; 1] = [(
        WIDGET_NATIVE_SET_TEXT_NAME,
        WIDGET_NATIVE_SET_TEXT_SIG,
        widget_native_set_text as *mut c_void,
    )];
    register_class_natives_best_effort(env, CHECK_BOX_CLASS, &check_box)?;

    let radio_button: [NativeBinding; 1] = [(
        RADIO_BUTTON_SET_TEXT_NAME,
        RADIO_BUTTON_SET_TEXT_SIG,
        radio_button_set_text as *mut c_void,
    )];
    register_class_natives_best_effort(env, RADIO_BUTTON_CLASS, &radio_button)?;

    let progress_bar: [NativeBinding; 2] = [
        (
            PROGRESS_BAR_SET_INDETERMINATE_NAME,
            PROGRESS_BAR_SET_INDETERMINATE_SIG,
            progress_bar_set_indeterminate as *mut c_void,
        ),
        (
            PROGRESS_NATIVE_SET_PROGRESS_NAME,
            PROGRESS_NATIVE_SET_PROGRESS_SIG,
            progress_native_set_progress as *mut c_void,
        ),
    ];
    register_class_natives_best_effort(env, PROGRESS_BAR_CLASS, &progress_bar)?;

    let seek_bar: [NativeBinding; 2] = [
        (
            PROGRESS_NATIVE_SET_PROGRESS_NAME,
            PROGRESS_NATIVE_SET_PROGRESS_SIG,
            progress_native_set_progress as *mut c_void,
        ),
        (
            SEEK_BAR_SET_MAX_NAME,
            SEEK_BAR_SET_MAX_SIG,
            seek_bar_set_max as *mut c_void,
        ),
    ];
    register_class_natives_best_effort(env, SEEK_BAR_CLASS, &seek_bar)?;

    let spinner: [NativeBinding; 1] = [(
        SPINNER_SET_ADAPTER_NAME,
        SPINNER_SET_ADAPTER_SIG,
        spinner_set_adapter as *mut c_void,
    )];
    register_class_natives_best_effort(env, SPINNER_CLASS, &spinner)?;

    let scroll_view: [NativeBinding; 2] = [
        (
            VIEW_GROUP_NATIVE_ADD_VIEW_NAME,
            VIEW_GROUP_NATIVE_ADD_VIEW_SIG,
            view_group_native_add_view as *mut c_void,
        ),
        (
            VIEW_GROUP_NATIVE_REMOVE_VIEW_NAME,
            VIEW_GROUP_NATIVE_REMOVE_VIEW_SIG,
            view_group_native_remove_view as *mut c_void,
        ),
    ];
    register_class_natives_best_effort(env, SCROLL_VIEW_CLASS, &scroll_view)?;

    tracing::info!(
        "registered Eclipse's non-GTK backing for the inflatable android.widget.* property setters \
         (Button/EditText/CheckBox/RadioButton text; EditText text/editor-action listeners RETAINED; \
         ProgressBar/SeekBar progress/indeterminate/max; Button compound-drawables; Spinner adapter; \
         ScrollView add/removeView; per-method best-effort)"
    );
    Ok(())
}

pub const DRAWABLE_CLASS: &JNIStr = jni_str!("android/graphics/drawable/Drawable");

const DRAWABLE_NATIVE_CONSTRUCTOR_NAME: &JNIStr = jni_str!("native_constructor");
const DRAWABLE_NATIVE_CONSTRUCTOR_SIG: &JNIStr = jni_str!("()J");

const DRAWABLE_NATIVE_UNREF_NAME: &JNIStr = jni_str!("native_unref");
const DRAWABLE_NATIVE_UNREF_SIG: &JNIStr = jni_str!("(J)V");

const DRAWABLE_NATIVE_INVALIDATE_NAME: &JNIStr = jni_str!("native_invalidate");
const DRAWABLE_NATIVE_INVALIDATE_SIG: &JNIStr = jni_str!("(J)V");

const DRAWABLE_NATIVE_REF_NAME: &JNIStr = jni_str!("native_ref");
const DRAWABLE_NATIVE_REF_SIG: &JNIStr = jni_str!("(J)V");
const DRAWABLE_NATIVE_DRAW_NAME: &JNIStr = jni_str!("native_draw");
const DRAWABLE_NATIVE_DRAW_SIG: &JNIStr = jni_str!("(JJII)V");
const DRAWABLE_PAINTABLE_FROM_PATH_NAME: &JNIStr = jni_str!("native_paintable_from_path");
const DRAWABLE_PAINTABLE_FROM_PATH_SIG: &JNIStr = jni_str!("(Ljava/lang/String;)J");

const DRAWABLE_CONTAINER_CLASS: &JNIStr = jni_str!("android/graphics/drawable/DrawableContainer");
const DRAWABLE_CONTAINER_SELECT_CHILD_NAME: &JNIStr = jni_str!("native_selectChild");
const DRAWABLE_CONTAINER_SELECT_CHILD_SIG: &JNIStr = jni_str!("(JJ)V");

const NINE_PATCH_DRAWABLE_CLASS: &JNIStr = jni_str!("android/graphics/drawable/NinePatchDrawable");
const NINE_PATCH_CREATE_NAME: &JNIStr = jni_str!("nativeCreate");
const NINE_PATCH_CREATE_FROM_PATH_SIG: &JNIStr = jni_str!("(Ljava/lang/String;)J");
const NINE_PATCH_CREATE_FROM_CHUNK_SIG: &JNIStr = jni_str!("([BJ)J");
const NINE_PATCH_SET_TINT_NAME: &JNIStr = jni_str!("nativeSetTint");
const NINE_PATCH_SET_TINT_SIG: &JNIStr = jni_str!("(JI)V");

const DRAWABLE_HANDLE_SENTINEL: jlong = 0x4452;

const DRAWABLE_CONTAINER_HANDLE_SENTINEL: jlong = 0x4443;

extern "system" fn drawable_native_constructor<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        tracing::debug!(
            target: "android.graphics.drawable.Drawable",
            handle = DRAWABLE_HANDLE_SENTINEL,
            "Drawable.native_constructor: returning non-GTK non-zero drawable sentinel (no draw pass)"
        );
        Ok(DRAWABLE_HANDLE_SENTINEL)
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn drawable_native_unref<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    native_ptr: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        release_paintable(native_ptr);
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

fn is_registry_paintable(paintable: jlong) -> bool {
    !matches!(
        paintable,
        0 | DRAWABLE_HANDLE_SENTINEL | DRAWABLE_CONTAINER_HANDLE_SENTINEL
    )
}

fn retain_paintable(paintable: jlong) {
    if !is_registry_paintable(paintable) {
        return;
    }
    match bitmap_registry::retain(paintable) {
        Ok(()) => tracing::trace!(
            target: "android.graphics.drawable.Drawable",
            paintable,
            "Drawable.native_ref: retained recorded paintable"
        ),
        Err(e) => tracing::debug!(
            target: "android.graphics.drawable.Drawable",
            paintable,
            error = %e,
            "Drawable.native_ref: dead paintable handle (ignored)"
        ),
    }
}

fn release_paintable(paintable: jlong) {
    if !is_registry_paintable(paintable) {
        return;
    }
    match bitmap_registry::release(paintable) {
        Ok(()) => tracing::trace!(
            target: "android.graphics.drawable.Drawable",
            paintable,
            "Drawable.native_unref: released recorded paintable"
        ),
        Err(e) => tracing::debug!(
            target: "android.graphics.drawable.Drawable",
            paintable,
            error = %e,
            "Drawable.native_unref: dead paintable handle (ignored)"
        ),
    }
}

extern "system" fn drawable_native_invalidate<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    paintable: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        tracing::trace!(
            target: "android.graphics.drawable.Drawable",
            paintable,
            "Drawable.native_invalidate: no-op (sentinel handle, draw-free lifecycle)"
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn drawable_native_ref<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    paintable: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        retain_paintable(paintable);
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn drawable_native_draw<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    paintable: jlong,
    snapshot: jlong,
    width: jint,
    height: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        tracing::trace!(
            target: "android.graphics.drawable.Drawable",
            paintable,
            snapshot,
            width,
            height,
            "Drawable.native_draw: no-op (headless recording; the engine renders the screen)"
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn drawable_native_paintable_from_path<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    path: JString<'local>,
) -> jlong {
    env.with_env(|env| -> jni::errors::Result<jlong> {
        if path.is_null() {
            return Ok(0);
        }
        let path = path.try_to_string(env)?;
        Ok(record_bitmap_from_file(
            &path,
            "Drawable.native_paintable_from_path",
        ))
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn drawable_container_native_constructor<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        tracing::debug!(
            target: "android.graphics.drawable.DrawableContainer",
            handle = DRAWABLE_CONTAINER_HANDLE_SENTINEL,
            "DrawableContainer.native_constructor: returning non-GTK non-zero container sentinel"
        );
        Ok(DRAWABLE_CONTAINER_HANDLE_SENTINEL)
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn drawable_container_native_select_child<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    container: jlong,
    child: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        tracing::trace!(
            target: "android.graphics.drawable.DrawableContainer",
            container,
            child,
            "DrawableContainer.native_selectChild: no-op (selection state lives in Java curIndex)"
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn nine_patch_native_create_from_path<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    path: JString<'local>,
) -> jlong {
    env.with_env(|env| -> jni::errors::Result<jlong> {
        if path.is_null() {
            return Ok(0);
        }
        let path = path.try_to_string(env)?;
        Ok(record_bitmap_from_file(
            &path,
            "NinePatchDrawable.nativeCreate",
        ))
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn nine_patch_native_create_from_chunk<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    _chunk: JByteArray<'local>,
    texture: jlong,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        match bitmap_registry::with_bitmap(texture, |_| ()) {
            Ok(()) => Ok(texture),
            Err(e) => {
                tracing::debug!(
                    target: "android.graphics.drawable.NinePatchDrawable",
                    texture,
                    error = %e,
                    "NinePatchDrawable.nativeCreate: dead texture handle → 0 (no paintable)"
                );
                Ok(0)
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn nine_patch_native_set_tint<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    paintable: jlong,
    tint: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        tracing::trace!(
            target: "android.graphics.drawable.NinePatchDrawable",
            paintable,
            tint,
            "NinePatchDrawable.nativeSetTint: no-op (headless recording)"
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

fn register_drawable_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let drawable_bindings: [NativeBinding; 6] = [
        (
            DRAWABLE_NATIVE_CONSTRUCTOR_NAME,
            DRAWABLE_NATIVE_CONSTRUCTOR_SIG,
            drawable_native_constructor as *mut c_void,
        ),
        (
            DRAWABLE_NATIVE_UNREF_NAME,
            DRAWABLE_NATIVE_UNREF_SIG,
            drawable_native_unref as *mut c_void,
        ),
        (
            DRAWABLE_NATIVE_INVALIDATE_NAME,
            DRAWABLE_NATIVE_INVALIDATE_SIG,
            drawable_native_invalidate as *mut c_void,
        ),
        (
            DRAWABLE_NATIVE_REF_NAME,
            DRAWABLE_NATIVE_REF_SIG,
            drawable_native_ref as *mut c_void,
        ),
        (
            DRAWABLE_NATIVE_DRAW_NAME,
            DRAWABLE_NATIVE_DRAW_SIG,
            drawable_native_draw as *mut c_void,
        ),
        (
            DRAWABLE_PAINTABLE_FROM_PATH_NAME,
            DRAWABLE_PAINTABLE_FROM_PATH_SIG,
            drawable_native_paintable_from_path as *mut c_void,
        ),
    ];
    let drawable_bound =
        register_class_natives_best_effort(env, DRAWABLE_CLASS, &drawable_bindings)?;

    let container_bindings: [NativeBinding; 2] = [
        (
            DRAWABLE_NATIVE_CONSTRUCTOR_NAME,
            DRAWABLE_NATIVE_CONSTRUCTOR_SIG,
            drawable_container_native_constructor as *mut c_void,
        ),
        (
            DRAWABLE_CONTAINER_SELECT_CHILD_NAME,
            DRAWABLE_CONTAINER_SELECT_CHILD_SIG,
            drawable_container_native_select_child as *mut c_void,
        ),
    ];
    let container_bound =
        register_class_natives_best_effort(env, DRAWABLE_CONTAINER_CLASS, &container_bindings)?;

    let nine_patch_bindings: [NativeBinding; 3] = [
        (
            NINE_PATCH_CREATE_NAME,
            NINE_PATCH_CREATE_FROM_PATH_SIG,
            nine_patch_native_create_from_path as *mut c_void,
        ),
        (
            NINE_PATCH_CREATE_NAME,
            NINE_PATCH_CREATE_FROM_CHUNK_SIG,
            nine_patch_native_create_from_chunk as *mut c_void,
        ),
        (
            NINE_PATCH_SET_TINT_NAME,
            NINE_PATCH_SET_TINT_SIG,
            nine_patch_native_set_tint as *mut c_void,
        ),
    ];
    let nine_patch_bound =
        register_class_natives_best_effort(env, NINE_PATCH_DRAWABLE_CLASS, &nine_patch_bindings)?;
    tracing::info!(
        drawable_bound,
        container_bound,
        nine_patch_bound,
        "registered Eclipse's non-GTK drawable paintable-lifecycle backing (Drawable.native_constructor + native_unref + native_invalidate + native_ref + native_draw + native_paintable_from_path; DrawableContainer.native_constructor + native_selectChild; NinePatchDrawable.nativeCreate ×2 + nativeSetTint) (per-method best-effort)"
    );
    Ok(())
}

const BITMAP_FACTORY_CLASS: &JNIStr = jni_str!("android/graphics/BitmapFactory");

const BITMAP_CLASS: &JNIStr = jni_str!("android/graphics/Bitmap");

const BITMAP_FACTORY_DECODE_STREAM_NAME: &JNIStr = jni_str!("nativeDecodeStream");
const BITMAP_FACTORY_DECODE_STREAM_SIG: &JNIStr = jni_str!(
    "(Ljava/io/InputStream;[BLandroid/graphics/Rect;Landroid/graphics/BitmapFactory$Options;)J"
);

const BITMAP_GET_WIDTH_NAME: &JNIStr = jni_str!("native_get_width");
const BITMAP_GET_WIDTH_SIG: &JNIStr = jni_str!("(J)I");
const BITMAP_GET_HEIGHT_NAME: &JNIStr = jni_str!("native_get_height");
const BITMAP_GET_HEIGHT_SIG: &JNIStr = jni_str!("(J)I");

const BITMAP_RECYCLE_NAME: &JNIStr = jni_str!("native_recycle");
const BITMAP_RECYCLE_SIG: &JNIStr = jni_str!("(JJ)V");
const BITMAP_CREATE_TEXTURE_NAME: &JNIStr = jni_str!("native_create_texture");
const BITMAP_CREATE_TEXTURE_SIG: &JNIStr = jni_str!("(JIIII)J");
const BITMAP_CREATE_SNAPSHOT_NAME: &JNIStr = jni_str!("native_create_snapshot");
const BITMAP_CREATE_SNAPSHOT_SIG: &JNIStr = jni_str!("(J)J");
const BITMAP_REF_TEXTURE_NAME: &JNIStr = jni_str!("native_ref_texture");
const BITMAP_REF_TEXTURE_SIG: &JNIStr = jni_str!("(J)J");

const BITMAP_DECODE_MAX_BYTES: usize = 64 * 1024 * 1024;

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    if bytes.get(..8)? != PNG_SIGNATURE {
        return None;
    }

    if bytes.get(12..16)? != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
    let height = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);
    Some((width, height))
}

fn record_bitmap_from_file(path: &str, caller: &str) -> jlong {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::debug!(
                target: "android.graphics.drawable.Drawable",
                path,
                caller,
                error = %e,
                "recorded-bitmap file read failed → 0 (no paintable)"
            );
            return 0;
        }
    };
    if bytes.len() > BITMAP_DECODE_MAX_BYTES {
        tracing::warn!(
            target: "android.graphics.drawable.Drawable",
            path,
            caller,
            len = bytes.len(),
            cap = BITMAP_DECODE_MAX_BYTES,
            "recorded-bitmap file exceeds the decode cap → 0 (no paintable)"
        );
        return 0;
    }
    let (width, height) = match png_dimensions(&bytes) {
        Some((w, h)) => (
            i32::try_from(w).unwrap_or(i32::MAX),
            i32::try_from(h).unwrap_or(i32::MAX),
        ),
        None => {
            tracing::warn!(
                target: "android.graphics.drawable.Drawable",
                path,
                caller,
                len = bytes.len(),
                "recorded-bitmap file has an unrecognized image encoding (recorded 0×0)"
            );
            (0, 0)
        }
    };
    match bitmap_registry::store_unowned(bitmap_registry::BitmapState {
        width,
        height,
        bytes,
    }) {
        Ok(handle) => {
            tracing::debug!(
                target: "android.graphics.drawable.Drawable",
                path,
                caller,
                handle,
                width,
                height,
                "recorded bitmap from file (headless; no raster)"
            );
            handle
        }
        Err(e) => {
            tracing::warn!(
                target: "android.graphics.drawable.Drawable",
                path,
                caller,
                error = %e,
                "recorded-bitmap registry store failed → 0"
            );
            0
        }
    }
}

extern "system" fn bitmap_factory_native_decode_stream<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    is: JObject<'local>,
    storage: JByteArray<'local>,
    _out_padding: JObject<'local>,
    _opts: JObject<'local>,
) -> jlong {
    env.with_env(|env| -> jni::errors::Result<jlong> {
        if is.is_null() {
            return Ok(0);
        }

        let buf = if !storage.is_null() && storage.len(env)? > 0 {
            storage
        } else {
            env.new_byte_array(8192)?
        };
        let buf_len = buf.len(env)?;
        let mut bytes: Vec<u8> = Vec::new();
        loop {
            let n = env
                .call_method(&is, jni_str!("read"), jni_sig!("([B)I"), &[JValue::Object(&buf)])?
                .i()?;
            if n <= 0 {
                break;
            }
            let n = usize::try_from(n).unwrap_or(0).min(buf_len);
            let mut chunk = vec![0i8; n];
            buf.get_region(env, 0, &mut chunk)?;
            bytes.extend(chunk.iter().map(|&b| u8::from_ne_bytes(b.to_ne_bytes())));
            if bytes.len() > BITMAP_DECODE_MAX_BYTES {
                tracing::warn!(
                    target: "android.graphics.BitmapFactory",
                    cap = BITMAP_DECODE_MAX_BYTES,
                    "BitmapFactory.nativeDecodeStream: stream exceeds the decode cap → 0 (no bitmap)"
                );
                return Ok(0);
            }
        }
        let (width, height) = match png_dimensions(&bytes) {
            Some((w, h)) => (
                i32::try_from(w).unwrap_or(i32::MAX),
                i32::try_from(h).unwrap_or(i32::MAX),
            ),
            None => {

                tracing::warn!(
                    target: "android.graphics.BitmapFactory",
                    len = bytes.len(),
                    "BitmapFactory.nativeDecodeStream: unrecognized image encoding (recorded 0×0)"
                );
                (0, 0)
            }
        };
        match bitmap_registry::store(bitmap_registry::BitmapState {
            width,
            height,
            bytes,
        }) {
            Ok(handle) => {
                tracing::debug!(
                    target: "android.graphics.BitmapFactory",
                    handle,
                    width,
                    height,
                    "BitmapFactory.nativeDecodeStream: recorded bitmap (headless; no raster)"
                );
                Ok(handle)
            }
            Err(e) => {
                tracing::warn!(
                    target: "android.graphics.BitmapFactory",
                    error = %e,
                    "BitmapFactory.nativeDecodeStream: registry store failed → 0"
                );
                Ok(0)
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn bitmap_native_get_width<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    texture: jlong,
) -> jint {
    env.with_env(|_env| -> jni::errors::Result<jint> {
        Ok(
            bitmap_registry::with_bitmap(texture, |s| s.width).unwrap_or_else(|e| {
                tracing::debug!(
                    target: "android.graphics.Bitmap",
                    texture,
                    error = %e,
                    "Bitmap.native_get_width: invalid bitmap handle → 0"
                );
                0
            }),
        )
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn bitmap_native_get_height<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    texture: jlong,
) -> jint {
    env.with_env(|_env| -> jni::errors::Result<jint> {
        Ok(
            bitmap_registry::with_bitmap(texture, |s| s.height).unwrap_or_else(|e| {
                tracing::debug!(
                    target: "android.graphics.Bitmap",
                    texture,
                    error = %e,
                    "Bitmap.native_get_height: invalid bitmap handle → 0"
                );
                0
            }),
        )
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn bitmap_native_recycle<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    texture: jlong,
    snapshot: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        for (label, handle) in [("texture", texture), ("snapshot", snapshot)] {
            if handle == 0 {
                continue;
            }
            match bitmap_registry::free(handle) {
                Ok(()) => tracing::trace!(
                    target: "android.graphics.Bitmap",
                    peer = label,
                    handle,
                    "Bitmap.native_recycle: freed recorded bitmap"
                ),
                Err(e) => tracing::debug!(
                    target: "android.graphics.Bitmap",
                    peer = label,
                    handle,
                    error = %e,
                    "Bitmap.native_recycle: dead handle (ignored)"
                ),
            }
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn bitmap_native_create_texture<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    snapshot: jlong,
    width: jint,
    height: jint,
    stride: jint,
    format: jint,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        if snapshot != 0 {
            return match bitmap_registry::with_bitmap(snapshot, |_| ()) {
                Ok(()) => {
                    tracing::trace!(
                        target: "android.graphics.Bitmap",
                        snapshot,
                        "Bitmap.native_create_texture: identity move (snapshot record becomes the texture)"
                    );
                    Ok(snapshot)
                }
                Err(e) => {
                    tracing::debug!(
                        target: "android.graphics.Bitmap",
                        snapshot,
                        error = %e,
                        "Bitmap.native_create_texture: dead snapshot handle → 0 (no texture)"
                    );
                    Ok(0)
                }
            };
        }
        match bitmap_registry::store(bitmap_registry::BitmapState {
            width,
            height,
            bytes: Vec::new(),
        }) {
            Ok(handle) => {
                tracing::debug!(
                    target: "android.graphics.Bitmap",
                    handle,
                    width,
                    height,
                    stride,
                    format,
                    "Bitmap.native_create_texture: recorded blank createBitmap surface (headless)"
                );
                Ok(handle)
            }
            Err(e) => {
                tracing::warn!(
                    target: "android.graphics.Bitmap",
                    error = %e,
                    "Bitmap.native_create_texture: registry store failed → 0"
                );
                Ok(0)
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn bitmap_native_create_snapshot<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    texture: jlong,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        if texture == 0 {
            return Ok(0);
        }
        match bitmap_registry::with_bitmap(texture, |_| ()) {
            Ok(()) => {
                tracing::trace!(
                    target: "android.graphics.Bitmap",
                    texture,
                    "Bitmap.native_create_snapshot: identity move (texture record becomes the snapshot)"
                );
                Ok(texture)
            }
            Err(e) => {
                tracing::debug!(
                    target: "android.graphics.Bitmap",
                    texture,
                    error = %e,
                    "Bitmap.native_create_snapshot: dead texture handle → 0 (no snapshot)"
                );
                Ok(0)
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn bitmap_native_ref_texture<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    texture: jlong,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        let cloned = bitmap_registry::with_bitmap(texture, |s| bitmap_registry::BitmapState {
            width: s.width,
            height: s.height,
            bytes: s.bytes.clone(),
        });
        match cloned {
            Ok(state) => match bitmap_registry::store(state) {
                Ok(handle) => {
                    tracing::debug!(
                        target: "android.graphics.Bitmap",
                        texture,
                        handle,
                        "Bitmap.native_ref_texture: duplicated recorded bitmap"
                    );
                    Ok(handle)
                }
                Err(e) => {
                    tracing::warn!(
                        target: "android.graphics.Bitmap",
                        texture,
                        error = %e,
                        "Bitmap.native_ref_texture: registry store failed → 0"
                    );
                    Ok(0)
                }
            },
            Err(e) => {
                tracing::debug!(
                    target: "android.graphics.Bitmap",
                    texture,
                    error = %e,
                    "Bitmap.native_ref_texture: dead texture handle → 0"
                );
                Ok(0)
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

fn register_bitmap_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let factory_bindings: [NativeBinding; 1] = [(
        BITMAP_FACTORY_DECODE_STREAM_NAME,
        BITMAP_FACTORY_DECODE_STREAM_SIG,
        bitmap_factory_native_decode_stream as *mut c_void,
    )];
    let factory_bound =
        register_class_natives_best_effort(env, BITMAP_FACTORY_CLASS, &factory_bindings)?;

    let bitmap_bindings: [NativeBinding; 6] = [
        (
            BITMAP_GET_WIDTH_NAME,
            BITMAP_GET_WIDTH_SIG,
            bitmap_native_get_width as *mut c_void,
        ),
        (
            BITMAP_GET_HEIGHT_NAME,
            BITMAP_GET_HEIGHT_SIG,
            bitmap_native_get_height as *mut c_void,
        ),
        (
            BITMAP_RECYCLE_NAME,
            BITMAP_RECYCLE_SIG,
            bitmap_native_recycle as *mut c_void,
        ),
        (
            BITMAP_CREATE_TEXTURE_NAME,
            BITMAP_CREATE_TEXTURE_SIG,
            bitmap_native_create_texture as *mut c_void,
        ),
        (
            BITMAP_CREATE_SNAPSHOT_NAME,
            BITMAP_CREATE_SNAPSHOT_SIG,
            bitmap_native_create_snapshot as *mut c_void,
        ),
        (
            BITMAP_REF_TEXTURE_NAME,
            BITMAP_REF_TEXTURE_SIG,
            bitmap_native_ref_texture as *mut c_void,
        ),
    ];
    let bitmap_bound = register_class_natives_best_effort(env, BITMAP_CLASS, &bitmap_bindings)?;
    tracing::info!(
        factory_bound,
        bitmap_bound,
        "registered Eclipse's non-GTK recorded-bitmap backing (BitmapFactory.nativeDecodeStream + Bitmap.native_get_width/height + native_recycle + native_create_texture + native_create_snapshot + native_ref_texture)"
    );
    Ok(())
}

pub const WINDOW_CLASS: &JNIStr = jni_str!("android/view/Window");

const WINDOW_SET_JOBJECT_NAME: &JNIStr = jni_str!("set_jobject");
const WINDOW_SET_JOBJECT_SIG: &JNIStr = jni_str!("(JLandroid/view/Window;)V");
const WINDOW_SET_TITLE_NAME: &JNIStr = jni_str!("set_title");
const WINDOW_SET_TITLE_SIG: &JNIStr = jni_str!("(JLjava/lang/String;)V");
const WINDOW_SET_LAYOUT_NAME: &JNIStr = jni_str!("set_layout");
const WINDOW_SET_LAYOUT_SIG: &JNIStr = jni_str!("(JII)V");
const WINDOW_SET_WIDGET_AS_ROOT_NAME: &JNIStr = jni_str!("set_widget_as_root");
const WINDOW_SET_WIDGET_AS_ROOT_SIG: &JNIStr = jni_str!("(JJ)V");

const WINDOW_REMOVE_GTK_BACKGROUND_NAME: &JNIStr = jni_str!("remove_gtk_background");
const WINDOW_REMOVE_GTK_BACKGROUND_SIG: &JNIStr = jni_str!("(J)V");
const WINDOW_SET_SCREEN_BRIGHTNESS_NAME: &JNIStr = jni_str!("set_screen_brightness");
const WINDOW_SET_SCREEN_BRIGHTNESS_SIG: &JNIStr = jni_str!("(F)V");
const WINDOW_TAKE_INPUT_QUEUE_NAME: &JNIStr = jni_str!("take_input_queue");
const WINDOW_TAKE_INPUT_QUEUE_SIG: &JNIStr =
    jni_str!("(JLandroid/view/InputQueue$Callback;Landroid/view/InputQueue;)V");

extern "system" fn window_set_jobject<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    ptr: jlong,
    window: JObject<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if window_registry::with_dialog(ptr, |_| ()).is_ok() {
            tracing::debug!(
                target: "android.view.Window",
                ptr,
                "Window.set_jobject: a dialog window does not pin its Window object"
            );
            return Ok(());
        }
        match env.new_global_ref(&window) {
            Ok(global) => match window_registry::set_jobject(ptr, global) {
                Ok(()) => tracing::debug!(
                    target: "android.view.Window",
                    ptr,
                    "Window.set_jobject: captured Java Window object on non-GTK window"
                ),
                Err(e) => tracing::debug!(
                    target: "android.view.Window",
                    ptr,
                    error = %e,
                    "Window.set_jobject: invalid window handle (Window object not captured)"
                ),
            },
            Err(e) => tracing::debug!(
                target: "android.view.Window",
                ptr,
                error = %e,
                "Window.set_jobject: new_global_ref failed (Window object not captured)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn window_set_title<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    native_window: jlong,
    title: JString<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let title_str = if title.is_null() {
            String::new()
        } else {
            title.try_to_string(env)?
        };
        match window_registry::with_window(native_window, |w| w.title = title_str.clone()) {
            Ok(()) => tracing::debug!(
                target: "android.view.Window",
                native_window,
                title = %title_str,
                "Window.set_title: stored window title (non-GTK)"
            ),
            Err(e) => tracing::debug!(
                target: "android.view.Window",
                native_window,
                error = %e,
                "Window.set_title: invalid window handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn window_set_layout<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    native_window: jlong,
    width: jint,
    height: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = window_registry::with_window(native_window, |_w| ()) {
            tracing::debug!(
                target: "android.view.Window",
                native_window,
                error = %e,
                "Window.set_layout: invalid window handle (ignored)"
            );
        } else {
            mark_global_layout_pending();
            tracing::trace!(
                target: "android.view.Window",
                native_window, width, height,
                "Window.set_layout: validated handle, no-op (layout deferred)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn window_remove_gtk_background<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    native_window: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = window_registry::with_window(native_window, |_w| ()) {
            tracing::debug!(
                target: "android.view.Window",
                native_window,
                error = %e,
                "Window.remove_gtk_background: invalid window handle (ignored)"
            );
        } else {
            tracing::trace!(
                target: "android.view.Window",
                native_window,
                "Window.remove_gtk_background: validated handle, no-op (non-GTK)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn window_set_screen_brightness<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    brightness: jfloat,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        tracing::trace!(
            target: "android.view.Window",
            brightness,
            "Window.set_screen_brightness: no-op (desktop brightness remains user-controlled)"
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn window_take_input_queue<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    native_window: jlong,
    _callback: JObject<'local>,
    _queue: JObject<'local>,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = window_registry::with_window(native_window, |_w| ()) {
            tracing::debug!(
                target: "android.view.Window",
                native_window,
                error = %e,
                "Window.take_input_queue: invalid window handle (ignored)"
            );
        } else {
            tracing::trace!(
                target: "android.view.Window",
                native_window,
                "Window.take_input_queue: no-op (input is dispatched by Eclipse's winit bridge)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn window_set_widget_as_root<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    native_window: jlong,
    widget: jlong,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let root = view_registry::with_view(widget, |_v| ())
            .is_ok()
            .then_some(widget);
        let recorded = match dialogs::set_dialog_root(env, native_window, root) {
            Ok(()) => Ok(false),
            Err(window_registry::WindowRegistryError::NotADialog) => {
                window_registry::with_window(native_window, |w| w.root_view = root).map(|()| true)
            }
            Err(e) => Err(e),
        };
        match recorded {
            Ok(activity_window) => {
                if activity_window {
                    show_window_root(root, true);
                    view_registry::set_active_root(root.unwrap_or(0));
                    crate::webview::client::refresh_visibility(env);
                }
                tracing::debug!(
                    target: "android.view.Window",
                    native_window,
                    widget,
                    view_ok = root.is_some(),
                    activity_window,
                    "Window.set_widget_as_root: recorded content-root view handle (non-GTK)"
                );
            }
            Err(e) => tracing::debug!(
                target: "android.view.Window",
                native_window,
                widget,
                error = %e,
                "Window.set_widget_as_root: invalid window handle (active root unchanged)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

fn show_window_root(root: Option<view_registry::ViewHandle>, shown: bool) {
    let Some(root) = root else {
        return;
    };
    let changed = if shown {
        view_registry::mark_window_root(root)
    } else {
        view_registry::release_window_root(root)
    };
    if let Err(error) = changed {
        tracing::warn!(
            target: "android.view.Window",
            root,
            shown,
            %error,
            "a window's content root is not a live view, so its WebView windows keep their state"
        );
    }
}

fn register_window_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(WINDOW_CLASS)?;
    let methods = [
        unsafe {
            NativeMethod::from_raw_parts(
                WINDOW_SET_JOBJECT_NAME,
                WINDOW_SET_JOBJECT_SIG,
                window_set_jobject as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                WINDOW_SET_TITLE_NAME,
                WINDOW_SET_TITLE_SIG,
                window_set_title as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                WINDOW_SET_LAYOUT_NAME,
                WINDOW_SET_LAYOUT_SIG,
                window_set_layout as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                WINDOW_SET_WIDGET_AS_ROOT_NAME,
                WINDOW_SET_WIDGET_AS_ROOT_SIG,
                window_set_widget_as_root as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                WINDOW_REMOVE_GTK_BACKGROUND_NAME,
                WINDOW_REMOVE_GTK_BACKGROUND_SIG,
                window_remove_gtk_background as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                WINDOW_SET_SCREEN_BRIGHTNESS_NAME,
                WINDOW_SET_SCREEN_BRIGHTNESS_SIG,
                window_set_screen_brightness as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                WINDOW_TAKE_INPUT_QUEUE_NAME,
                WINDOW_TAKE_INPUT_QUEUE_SIG,
                window_take_input_queue as *mut std::ffi::c_void,
            )
        },
    ];

    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/view/Window",
        "registered Eclipse's non-GTK backing for Window.set_jobject + set_title + set_layout + set_widget_as_root + remove_gtk_background + set_screen_brightness + take_input_queue"
    );
    Ok(())
}

const ACTIVITY_NATIVE_START_ACTIVITY_NAME: &JNIStr = jni_str!("nativeStartActivity");
const ACTIVITY_NATIVE_START_ACTIVITY_SIG: &JNIStr = jni_str!("(Landroid/app/Activity;)V");
const ACTIVITY_NATIVE_FINISH_NAME: &JNIStr = jni_str!("nativeFinish");
const ACTIVITY_NATIVE_FINISH_SIG: &JNIStr = jni_str!("(J)V");
const ACTIVITY_NATIVE_RESUME_ACTIVITY_NAME: &JNIStr = jni_str!("nativeResumeActivity");
const ACTIVITY_NATIVE_RESUME_ACTIVITY_SIG: &JNIStr =
    jni_str!("(Ljava/lang/Class;Landroid/content/Intent;)Z");
const ACTIVITY_IS_IN_MULTI_WINDOW_MODE_NAME: &JNIStr = jni_str!("isInMultiWindowMode");
const ACTIVITY_IS_IN_MULTI_WINDOW_MODE_SIG: &JNIStr = jni_str!("()Z");
const ACTIVITY_IS_TASK_ROOT_NAME: &JNIStr = jni_str!("isTaskRoot");
const ACTIVITY_IS_TASK_ROOT_SIG: &JNIStr = jni_str!("()Z");
const ACTIVITY_FINISHING_FIELD_NAME: &JNIStr = jni_str!("finishing");
const BOOLEAN_FIELD_SIG: &JNIStr = jni_str!("Z");

enum TrackedActivity {
    Live(Global<JObject<'static>>),

    Finished(Weak<JObject<'static>>),
}

impl TrackedActivity {
    fn live(&self) -> Option<&Global<JObject<'static>>> {
        match self {
            Self::Live(live) => Some(live),
            Self::Finished(_) => None,
        }
    }

    fn refers_to(&self, env: &Env, activity: &JObject) -> bool {
        let same = match self {
            Self::Live(live) => env.is_same_object(live, activity),
            Self::Finished(finished) => env.is_same_object(finished, activity),
        };
        same.unwrap_or_else(|e| {
            tracing::debug!(
                target: "android.app.Activity",
                error = %e,
                "IsSameObject failed during finish lookup (entry skipped)"
            );
            false
        })
    }

    fn is_collected(&self, env: &Env) -> bool {
        match self {
            Self::Live(_) => false,
            Self::Finished(finished) => finished.is_garbage_collected(env).unwrap_or(false),
        }
    }
}

static TRACKED_ACTIVITIES: std::sync::Mutex<Vec<TrackedActivity>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
static ACTIVITY_TRACKER_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn track_activity(env: &Env, activity: &JObject) {
    match env.new_global_ref(activity) {
        Ok(global) => match TRACKED_ACTIVITIES.lock() {
            Ok(mut tracker) => tracker.push(TrackedActivity::Live(global)),
            Err(e) => tracing::warn!(
                target: "android.app.Activity",
                error = %e,
                "activity tracker poisoned: activity untracked"
            ),
        },
        Err(e) => tracing::warn!(
            target: "android.app.Activity",
            error = %e,
            "new_global_ref failed: activity untracked"
        ),
    }
}

fn mark_activity_finished_once(env: &mut Env, activity: &JObject) -> bool {
    let mut tracker = match TRACKED_ACTIVITIES.lock() {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(
                target: "android.app.Activity",
                error = %e,
                "activity tracker poisoned: finish dedupe unavailable"
            );
            return true;
        }
    };
    tracker.retain(|entry| !entry.is_collected(env));
    let position = tracker
        .iter()
        .position(|entry| entry.refers_to(env, activity));
    if let Some(TrackedActivity::Finished(_)) = position.map(|index| &tracker[index]) {
        return false;
    }
    let finished = match env.new_weak_ref(activity) {
        Ok(weak) => Some(TrackedActivity::Finished(weak)),
        Err(e) => {
            tracing::warn!(
                target: "android.app.Activity",
                error = %e,
                "new_weak_ref failed: finish-once guard not recorded"
            );
            None
        }
    };
    match (position, finished) {
        (Some(index), Some(entry)) => tracker[index] = entry,
        (Some(index), None) => {
            tracker.remove(index);
        }
        (None, Some(entry)) => tracker.push(entry),
        (None, None) => {}
    }
    true
}

pub fn dispatch_back_to_active_activity(vm: &Vm) -> Result<bool, FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| dispatch_back_to_latest_activity(env))) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

fn dispatch_back_to_latest_activity(env: &mut Env) -> Result<bool, FrameworkError> {
    let activity = {
        let tracker = TRACKED_ACTIVITIES
            .lock()
            .map_err(|_| FrameworkError::ActivityTrackerPoisoned)?;
        let Some(live) = tracker.iter().rev().find_map(TrackedActivity::live) else {
            return Ok(false);
        };
        checked(env, "Activity Back NewLocalRef", |env| {
            env.new_local_ref(live.as_obj())
        })?
    };
    let class_name = view_class_name(env, &activity).unwrap_or_default();
    checked(env, "Activity.onBackPressed", |env| {
        env.call_method(&activity, jni_str!("onBackPressed"), jni_sig!("()V"), &[])?
            .v()
    })?;
    tracing::info!(
        target: "android.app.Activity",
        class = %class_name,
        "desktop Back dispatched to Activity.onBackPressed"
    );
    Ok(true)
}

extern "system" fn activity_native_start_activity<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    activity: JObject<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if activity.is_null() {
            tracing::warn!(
                target: "android.app.Activity",
                "Activity.nativeStartActivity: null activity (ignored)"
            );
            return Ok(());
        }
        let class_name = view_class_name(env, &activity).unwrap_or_default();
        tracing::info!(
            target: "android.app.Activity",
            class = %class_name,
            "Activity.nativeStartActivity: driving the started activity to RESUMED (steps 5–7)"
        );
        track_activity(env, &activity);

        if call_activity_on_create(env, &activity, "nativeStartActivity Activity.onCreate").is_err()
            || call_activity_on_post_create(
                env,
                &activity,
                "nativeStartActivity Activity.onPostCreate",
            )
            .is_err()
            || call_activity_on_start(env, &activity, "nativeStartActivity Activity.onStart")
                .is_err()
            || call_activity_on_resume(env, &activity, "nativeStartActivity Activity.onResume")
                .is_err()
        {
            return Ok(());
        }
        tracing::info!(
            target: "android.app.Activity",
            class = %class_name,
            "Activity.nativeStartActivity: started activity resumed (steps 5–7 driven)"
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn activity_native_finish<'local>(
    mut env: EnvUnowned<'local>,
    this: JObject<'local>,
    native_window: jlong,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if this.is_null() {
            tracing::warn!(
                target: "android.app.Activity",
                handle = native_window,
                "Activity.nativeFinish: null receiver (ignored)"
            );
            return Ok(());
        }
        if !mark_activity_finished_once(env, &this) {
            tracing::debug!(
                target: "android.app.Activity",
                handle = native_window,
                "Activity.nativeFinish: down-lifecycle already driven (second queued finish — no-op)"
            );
            return Ok(());
        }
        let class_name = view_class_name(env, &this).unwrap_or_default();
        tracing::info!(
            target: "android.app.Activity",
            class = %class_name,
            handle = native_window,
            "Activity.nativeFinish: driving the finishing activity down (onPause → onStop → onDestroy)"
        );

        if let Err(error) = drive_activity_down_lifecycle(env, &this) {
            tracing::error!(
                target: "android.app.Activity",
                class = %class_name,
                error = %error,
                "Activity.nativeFinish: driving the finishing activity down failed"
            );
        }
        match activity_content_root(env, &this) {
            Ok(Some(root)) => {
                if view_registry::release_window_root(root).is_ok() {
                    crate::webview::client::refresh_visibility(env);
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(
                target: "android.app.Activity",
                class = %class_name,
                error = %e,
                "Activity.nativeFinish: the finishing activity's content root is unknown, so \
                 WebViews in it keep their windows"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn activity_native_resume_activity<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    cls: JObject<'local>,
    _intent: JObject<'local>,
) -> jboolean {
    env.with_env(|env| -> jni::errors::Result<jboolean> {
        if cls.is_null() {
            return Ok(false);
        }

        let cls = env.cast_local::<JClass>(cls)?;

        let target: Option<JObject<'_>> = {
            let tracker = match TRACKED_ACTIVITIES.lock() {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(
                        target: "android.app.Activity",
                        error = %e,
                        "activity tracker poisoned: nativeResumeActivity reports no live instance"
                    );
                    return Ok(false);
                }
            };
            let mut found = None;
            for live in tracker.iter().filter_map(TrackedActivity::live) {
                match env.is_instance_of(live.as_obj(), &cls) {
                    Ok(true) => {
                        found = Some(env.new_local_ref(live.as_obj())?);
                        break;
                    }
                    Ok(false) => {}
                    Err(e) => tracing::debug!(
                        target: "android.app.Activity",
                        error = %e,
                        "IsInstanceOf failed during resume lookup (entry skipped)"
                    ),
                }
            }
            found
        };
        match target {
            Some(activity) => {
                let class_name = view_class_name(env, &activity).unwrap_or_default();
                tracing::info!(
                    target: "android.app.Activity",
                    class = %class_name,
                    "Activity.nativeResumeActivity: live instance found — driving to RESUMED"
                );

                let _ = call_activity_on_resume(
                    env,
                    &activity,
                    "nativeResumeActivity Activity.onResume",
                );
                Ok(true)
            }
            None => Ok(false),
        }
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn activity_is_in_multi_window_mode<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
) -> jboolean {
    env.with_env(|_env| -> jni::errors::Result<jboolean> { Ok(false) })
        .resolve::<LogErrorAndDefault>()
}

extern "system" fn activity_is_task_root<'local>(
    mut env: EnvUnowned<'local>,
    this: JObject<'local>,
) -> jboolean {
    env.with_env(|env| -> jni::errors::Result<jboolean> {
        let tracker = match TRACKED_ACTIVITIES.lock() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    target: "android.app.Activity",
                    error = %e,
                    "activity tracker poisoned: isTaskRoot reports false"
                );
                return Ok(false);
            }
        };
        Ok(tracker
            .iter()
            .find_map(TrackedActivity::live)
            .is_some_and(|root| env.is_same_object(root.as_obj(), &this).unwrap_or(false)))
    })
    .resolve::<LogErrorAndDefault>()
}

fn register_activity_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(ACTIVITY_CLASS)?;
    let methods = [
        unsafe {
            NativeMethod::from_raw_parts(
                ACTIVITY_NATIVE_START_ACTIVITY_NAME,
                ACTIVITY_NATIVE_START_ACTIVITY_SIG,
                activity_native_start_activity as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                ACTIVITY_NATIVE_FINISH_NAME,
                ACTIVITY_NATIVE_FINISH_SIG,
                activity_native_finish as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                ACTIVITY_NATIVE_RESUME_ACTIVITY_NAME,
                ACTIVITY_NATIVE_RESUME_ACTIVITY_SIG,
                activity_native_resume_activity as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                ACTIVITY_IS_IN_MULTI_WINDOW_MODE_NAME,
                ACTIVITY_IS_IN_MULTI_WINDOW_MODE_SIG,
                activity_is_in_multi_window_mode as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                ACTIVITY_IS_TASK_ROOT_NAME,
                ACTIVITY_IS_TASK_ROOT_SIG,
                activity_is_task_root as *mut std::ffi::c_void,
            )
        },
    ];

    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/app/Activity",
        "registered Eclipse's backing for nativeStartActivity + nativeFinish + nativeResumeActivity + isInMultiWindowMode + isTaskRoot"
    );
    Ok(())
}

pub const CONTEXT_CLASS: &JNIStr = jni_str!("android/content/Context");

pub const APPLICATION_CLASS: &JNIStr = jni_str!("android/app/Application");

pub const CONTENT_PROVIDER_CLASS: &JNIStr = jni_str!("android/content/ContentProvider");

pub const LOOPER_CLASS: &JNIStr = jni_str!("android/os/Looper");

pub const STEP1_CREATE_APPLICATION: RecipeStep = RecipeStep {
    class: "android/content/Context",
    method: "createApplication",
    descriptor: "(J)Landroid/app/Application;",
};

pub const STEP2_CREATE_CONTENT_PROVIDERS: RecipeStep = RecipeStep {
    class: "android/content/ContentProvider",
    method: "createContentProviders",
    descriptor: "()V",
};

pub const STEP3_APPLICATION_ON_CREATE: RecipeStep = RecipeStep {
    class: "android/app/Application",
    method: "onCreate",
    descriptor: "()V",
};

pub const STEP4_CREATE_MAIN_ACTIVITY: RecipeStep = RecipeStep {
    class: "android/app/Activity",
    method: "createMainActivity",
    descriptor: "(Ljava/lang/String;JLjava/lang/String;)Landroid/app/Activity;",
};

pub const STEP5_ACTIVITY_ON_CREATE: RecipeStep = RecipeStep {
    class: "android/app/Activity",
    method: "onCreate",
    descriptor: "(Landroid/os/Bundle;)V",
};

pub const STEP_ACTIVITY_ON_POST_CREATE: RecipeStep = RecipeStep {
    class: "android/app/Activity",
    method: "onPostCreate",
    descriptor: "(Landroid/os/Bundle;)V",
};

pub const STEP6_ACTIVITY_ON_START: RecipeStep = RecipeStep {
    class: "android/app/Activity",
    method: "onStart",
    descriptor: "()V",
};

pub const STEP7_ACTIVITY_ON_RESUME: RecipeStep = RecipeStep {
    class: "android/app/Activity",
    method: "onResume",
    descriptor: "()V",
};

pub const ACTIVITY_CLASS: &JNIStr = jni_str!("android/app/Activity");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecipeStep {
    pub class: &'static str,

    pub method: &'static str,

    pub descriptor: &'static str,
}

#[derive(Clone, Copy)]
pub enum ActivityStart<'a> {
    Launcher(&'a str),
    View { activity: &'a str, uri: &'a str },
}

impl<'a> ActivityStart<'a> {
    pub fn activity(self) -> &'a str {
        match self {
            Self::Launcher(activity) | Self::View { activity, .. } => activity,
        }
    }

    fn uri(self) -> Option<&'a str> {
        match self {
            Self::Launcher(_) => None,
            Self::View { uri, .. } => Some(uri),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleProgress {
    BridgeProven,

    ApplicationOnCreate,

    ActivityOnCreate,

    ActivityResumed,
}

unsafe extern "C" {

    fn eclipse_art_load_native_library(
        load_fn: *mut c_void,
        vm: *mut c_void,
        env: *mut c_void,
        path: *const c_char,
        class_loader: *mut c_void,
        caller_class: *mut c_void,
        err_buf: *mut c_char,
        err_cap: usize,
    ) -> c_int;
}

const ART_LOAD_NATIVE_LIBRARY_SYMBOL: &[u8] =
    b"_ZN3art9JavaVMExt17LoadNativeLibraryEP7_JNIEnvRKNSt7__cxx1112basic_stringIcSt11char_traitsIcESaIcEEEP8_jobjectP7_jclassPS8_\0";

fn art_load_native_library_fn() -> Option<*mut c_void> {
    static FN: OnceLock<usize> = OnceLock::new();
    let addr = *FN.get_or_init(|| {
        let p = unsafe {
            libc::dlsym(
                libc::RTLD_DEFAULT,
                ART_LOAD_NATIVE_LIBRARY_SYMBOL.as_ptr() as *const c_char,
            )
        };
        p as usize
    });
    (addr != 0).then_some(addr as *mut c_void)
}

fn soname_from_load_path(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

const RUNTIME_CLASS: &JNIStr = jni_str!("java/lang/Runtime");

const NATIVE_LOAD_NAME: &JNIStr = jni_str!("nativeLoad");
const NATIVE_LOAD_SIG: &JNIStr =
    jni_str!("(Ljava/lang/String;Ljava/lang/ClassLoader;Ljava/lang/Class;)Ljava/lang/String;");

extern "system" fn runtime_native_load<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    filename: JString<'local>,
    loader: JObject<'local>,
    caller: JClass<'local>,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {

        let path = if filename.is_null() {
            String::new()
        } else {
            filename.try_to_string(env)?
        };

        let soname = soname_from_load_path(&path);
        if !path.is_empty() && crate::loader::engine::is_preloaded(soname) {
            if let Some(error) = preloaded_library::initialize_in_app_class_loader(env, soname)? {
                tracing::error!(
                    soname,
                    "Runtime.nativeLoad: the preloaded library failed its JNI initialization"
                );
                return Ok(error);
            }
            tracing::info!(
                soname,
                "Runtime.nativeLoad: preloaded by Eclipse's Rust loader; JNI_OnLoad and its natives are bound now, as on Android"
            );
            return Ok(JString::default());
        }

        let Some(load_fn) = art_load_native_library_fn() else {

            return env.new_string(format!(
                "Eclipse: cannot load \"{path}\": ART JavaVMExt::LoadNativeLibrary not found (is libart RTLD_GLOBAL?)"
            ));
        };
        let java_vm = env.get_java_vm()?.get_raw() as *mut c_void;
        let raw_env = env.get_raw() as *mut c_void;
        let loader_raw = loader.as_raw() as *mut c_void;
        let caller_raw = caller.as_raw() as *mut c_void;

        let c_path = match CString::new(path.as_str()) {
            Ok(s) => s,
            Err(_) => return env.new_string(format!("Eclipse: invalid library path \"{path}\"")),
        };
        let mut err_buf = [0u8; 1024];

        let ok = unsafe {
            eclipse_art_load_native_library(
                load_fn,
                java_vm,
                raw_env,
                c_path.as_ptr(),
                loader_raw,
                caller_raw,
                err_buf.as_mut_ptr() as *mut c_char,
                err_buf.len(),
            )
        };
        if ok == 1 {
            Ok(JString::default())
        } else {

            let end = err_buf.iter().position(|&b| b == 0).unwrap_or(err_buf.len());
            let msg = String::from_utf8_lossy(&err_buf[..end]).into_owned();
            let msg = if msg.is_empty() {
                format!("Eclipse: failed to load \"{path}\"")
            } else {
                msg
            };
            env.new_string(msg)
        }
    })
    .resolve::<LogErrorAndDefault>()
}

fn register_runtime_native_load_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(RUNTIME_CLASS)?;
    let methods = [unsafe {
        NativeMethod::from_raw_parts(
            NATIVE_LOAD_NAME,
            NATIVE_LOAD_SIG,
            runtime_native_load as *mut c_void,
        )
    }];

    match unsafe { env.register_native_methods(&class, &methods) } {
        Ok(()) => {
            tracing::info!(
                class = "java/lang/Runtime",
                "registered Eclipse's Runtime.nativeLoad interception (pre-loaded libs skip apkenv; others delegate to ART's LoadNativeLibrary)"
            );
        }
        Err(e) => {
            if env.exception_check() {
                env.exception_clear();
            }
            tracing::warn!(
                class = "java/lang/Runtime",
                error = %e,
                "could not register Runtime.nativeLoad interception (signature mismatch?); apkenv path unchanged — engine libs still pre-loaded"
            );
        }
    }
    Ok(())
}

pub fn drive_application_lifecycle(
    vm: &Vm,
    apk_path: &str,
    native_library_dir: &std::path::Path,
    signing_certificate_history: &SigningCertificateHistory,
    start: ActivityStart<'_>,
) -> Result<LifecycleProgress, FrameworkError> {
    let native_library_dir = native_library_dir
        .to_str()
        .ok_or_else(|| FrameworkError::NativeLibraryDirNotUtf8(native_library_dir.to_owned()))?;
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }
    let java_vm = unsafe { JavaVM::from_raw(raw) };

    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| {
            drive_lifecycle(
                env,
                apk_path,
                native_library_dir,
                signing_certificate_history,
                start,
            )
        })) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

pub fn register_engine_preload_natives(vm: &Vm) -> Result<(), FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| register_jni_onload_natives(env))) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

fn register_jni_onload_natives(env: &mut Env) -> Result<(), FrameworkError> {
    register_log_natives(env)?;
    register_process_natives(env)?;
    register_system_clock_natives(env)
}

fn prepare_main_looper_inner(env: &mut Env) -> Result<(), FrameworkError> {
    let looper_class = env.find_class(LOOPER_CLASS)?;
    checked(env, "step 0 Looper.prepareMainLooper", |env| {
        env.call_static_method(
            &looper_class,
            jni_str!("prepareMainLooper"),
            jni_sig!("()V"),
            &[],
        )?
        .v()
    })?;
    initialize_main_looper_jni_cache(env, &looper_class)?;
    crate::loader::native_provider::prepare_main_thread_looper(wake_main_looper)
        .map_err(FrameworkError::MainThreadLooper)?;

    let _ = MAIN_THREAD_ID.set(std::thread::current().id());
    Ok(())
}

struct MainLooperJniCache {
    queue: Global<JObject<'static>>,
    queue_next: JMethodID,
    message_get_target: JMethodID,
    handler_dispatch_message: JMethodID,
    message_recycle: JMethodID,

    _queue_class: Global<JClass<'static>>,
    _message_class: Global<JClass<'static>>,
    _handler_class: Global<JClass<'static>>,
}

static MAIN_LOOPER_JNI_CACHE: OnceLock<MainLooperJniCache> = OnceLock::new();

fn initialize_main_looper_jni_cache(
    env: &mut Env,
    looper_class: &JClass,
) -> Result<(), FrameworkError> {
    let queue = checked(env, "Looper.myQueue for cached main pump", |env| {
        env.call_static_method(
            looper_class,
            jni_str!("myQueue"),
            jni_sig!("()Landroid/os/MessageQueue;"),
            &[],
        )?
        .l()
    })?;
    let queue_class = env.get_object_class(&queue)?;
    let message_class = env.find_class(jni_str!("android/os/Message"))?;
    let handler_class = env.find_class(jni_str!("android/os/Handler"))?;

    let cache = MainLooperJniCache {
        queue_next: env.get_method_id(
            &queue_class,
            jni_str!("next"),
            jni_sig!("()Landroid/os/Message;"),
        )?,
        message_get_target: env.get_method_id(
            &message_class,
            jni_str!("getTarget"),
            jni_sig!("()Landroid/os/Handler;"),
        )?,
        handler_dispatch_message: env.get_method_id(
            &handler_class,
            jni_str!("dispatchMessage"),
            jni_sig!("(Landroid/os/Message;)V"),
        )?,
        message_recycle: env.get_method_id(&message_class, jni_str!("recycle"), jni_sig!("()V"))?,
        queue: env.new_global_ref(&queue)?,
        _queue_class: env.new_global_ref(&queue_class)?,
        _message_class: env.new_global_ref(&message_class)?,
        _handler_class: env.new_global_ref(&handler_class)?,
    };

    let _ = MAIN_LOOPER_JNI_CACHE.set(cache);
    Ok(())
}

pub fn prepare_main_looper(vm: &Vm) -> Result<(), FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| {
            register_system_clock_natives(env)?;
            register_message_queue_natives(env)?;
            prepare_main_looper_inner(env)
        })) {
            Ok(r) => r,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostWake {
    MainLooper,
    Control,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MainLooperDue {
    Now,
    At(Instant),
    WhenWoken,
}

impl MainLooperDue {
    pub fn deadline(self, now: Instant) -> Option<Instant> {
        match self {
            Self::Now => Some(now),
            Self::At(at) => Some(at),
            Self::WhenWoken => None,
        }
    }
}

static MAIN_LOOPER_WAKER: OnceLock<winit::event_loop::EventLoopProxy<HostWake>> = OnceLock::new();

static MAIN_LOOPER_WAKE_SENT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn install_main_looper_waker(proxy: winit::event_loop::EventLoopProxy<HostWake>) {
    if MAIN_LOOPER_WAKER.set(proxy).is_err() {
        tracing::warn!("main Looper waker already installed; keeping the first event loop's proxy");
    }
}

pub(crate) fn wake_main_looper() {
    let Some(proxy) = MAIN_LOOPER_WAKER.get() else {
        return;
    };
    if MAIN_LOOPER_WAKE_SENT.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return;
    }
    if proxy.send_event(HostWake::MainLooper).is_err() {
        tracing::debug!("main Looper wake dropped: the host event loop has exited");
    }
}

static ENGINE_PRESENT_WAKES_MAIN_LOOP: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(crate) fn set_engine_present_wakes_main_loop(wake: bool) {
    ENGINE_PRESENT_WAKES_MAIN_LOOP.store(wake, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn engine_presented() {
    wake_when_armed(&ENGINE_PRESENT_WAKES_MAIN_LOOP, wake_main_looper);
}

fn wake_when_armed(armed: &std::sync::atomic::AtomicBool, wake: impl FnOnce()) {
    if armed.load(std::sync::atomic::Ordering::Relaxed) {
        wake();
    }
}

static PENDING_HOST_CLIPBOARD_TEXT: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

pub(crate) fn take_pending_host_clipboard_text() -> Option<String> {
    PENDING_HOST_CLIPBOARD_TEXT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
}

fn queue_host_clipboard_text(text: String) {
    *PENDING_HOST_CLIPBOARD_TEXT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(text);
    wake_main_looper();
}

const CLIPBOARD_MANAGER_CLASS: &JNIStr = jni_str!("android/content/ClipboardManager");
const CLIPBOARD_MANAGER_SET_CLIPBOARD_NAME: &JNIStr = jni_str!("native_set_clipboard");
const CLIPBOARD_MANAGER_SET_CLIPBOARD_SIG: &JNIStr = jni_str!("(Ljava/lang/String;)V");

extern "system" fn clipboard_manager_native_set_clipboard<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    text: JString<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if text.is_null() {
            tracing::debug!(
                target: "android.content.ClipboardManager",
                "ClipboardManager.native_set_clipboard: clip without text (ignored)"
            );
            return Ok(());
        }
        queue_host_clipboard_text(text.try_to_string(env)?);
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

fn register_clipboard_manager_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let bindings: [NativeBinding; 1] = [(
        CLIPBOARD_MANAGER_SET_CLIPBOARD_NAME,
        CLIPBOARD_MANAGER_SET_CLIPBOARD_SIG,
        clipboard_manager_native_set_clipboard as *mut c_void,
    )];
    register_class_natives_best_effort(env, CLIPBOARD_MANAGER_CLASS, &bindings)?;
    Ok(())
}

pub fn pump_main_looper(vm: &Vm) -> Result<MainLooperDue, FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| run_main_looper_once(env))) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

fn run_main_looper_once(env: &mut Env) -> Result<MainLooperDue, FrameworkError> {
    let already = MAIN_LOOPER_PUMP_IN_PROGRESS
        .try_with(|f| f.replace(true))
        .unwrap_or(true);
    if already {
        return Ok(MainLooperDue::Now);
    }

    MAIN_LOOPER_WAKE_SENT.swap(false, std::sync::atomic::Ordering::AcqRel);
    crate::loader::native_provider::dispatch_main_thread_looper();
    run_pending_main_upcall(env);
    let result = drive_main_messages(env);
    let layout_result = dispatch_pending_global_layout(env);
    let _ = MAIN_LOOPER_PUMP_IN_PROGRESS.try_with(|f| f.set(false));
    let due = result?;
    layout_result?;

    if !MAIN_LOOPER_PUMP_ACTIVE.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::info!(
            "main Looper pump active: dispatching main-thread messages from the winit loop"
        );
    }
    Ok(due)
}

const MAIN_LOOPER_MESSAGE_BUDGET: usize = 512;

fn main_looper_due_after_drain() -> MainLooperDue {
    match message_queue::take_main_yield_deadline() {
        Some(at) => MainLooperDue::At(at),
        None => MainLooperDue::WhenWoken,
    }
}

fn drive_main_messages(env: &mut Env) -> Result<MainLooperDue, FrameworkError> {
    if let Some(cache) = MAIN_LOOPER_JNI_CACHE.get() {
        return drive_main_messages_cached(env, cache);
    }

    let looper_class = env.find_class(LOOPER_CLASS)?;

    let queue = checked(env, "Looper.myQueue", |env| {
        env.call_static_method(
            &looper_class,
            jni_str!("myQueue"),
            jni_sig!("()Landroid/os/MessageQueue;"),
            &[],
        )?
        .l()
    })?;
    for _ in 0..MAIN_LOOPER_MESSAGE_BUDGET {
        let processed = env.with_local_frame(16, |env| -> Result<bool, FrameworkError> {

            let msg = checked(env, "MessageQueue.next", |env| {
                env.call_method(
                    &queue,
                    jni_str!("next"),
                    jni_sig!("()Landroid/os/Message;"),
                    &[],
                )?
                .l()
            })?;
            if msg.is_null() {
                return Ok(false);
            }

            let target = checked(env, "Message.getTarget", |env| {
                env.call_method(
                    &msg,
                    jni_str!("getTarget"),
                    jni_sig!("()Landroid/os/Handler;"),
                    &[],
                )?
                .l()
            })?;
            if !target.is_null() {
                if let Err(e) = checked(env, "Handler.dispatchMessage", |env| {
                    env.call_method(
                        &target,
                        jni_str!("dispatchMessage"),
                        jni_sig!("(Landroid/os/Message;)V"),
                        &[JValue::Object(&msg)],
                    )?
                    .v()
                }) {
                    tracing::debug!(error = %e, "main-looper message handler threw (cleared, continuing)");
                }
            }

            if let Err(e) = checked(env, "Message.recycle", |env| {
                env.call_method(&msg, jni_str!("recycle"), jni_sig!("()V"), &[])?
                    .v()
            }) {
                tracing::debug!(error = %e, "Message.recycle failed (ignored)");
            }
            Ok(true)
        })?;
        if !processed {
            return Ok(main_looper_due_after_drain());
        }
    }
    Ok(MainLooperDue::Now)
}

fn drive_main_messages_cached(
    env: &mut Env,
    cache: &MainLooperJniCache,
) -> Result<MainLooperDue, FrameworkError> {
    for _ in 0..MAIN_LOOPER_MESSAGE_BUDGET {
        let processed = env.with_local_frame(16, |env| -> Result<bool, FrameworkError> {
            let msg = checked(env, "MessageQueue.next (cached)", |env| {

                unsafe {
                    env.call_method_unchecked(
                        cache.queue.as_obj(),
                        cache.queue_next,
                        JavaType::Object,
                        &[],
                    )
                }?
                .l()
            })?;
            if msg.is_null() {
                return Ok(false);
            }

            let target = checked(env, "Message.getTarget (cached)", |env| {

                unsafe {
                    env.call_method_unchecked(
                        &msg,
                        cache.message_get_target,
                        JavaType::Object,
                        &[],
                    )
                }?
                .l()
            })?;
            if !target.is_null() {
                let args = [JValue::Object(&msg).as_jni()];
                if let Err(e) = checked(env, "Handler.dispatchMessage (cached)", |env| {

                    unsafe {
                        env.call_method_unchecked(
                            &target,
                            cache.handler_dispatch_message,
                            JavaType::Primitive(Primitive::Void),
                            &args,
                        )
                    }?
                    .v()
                }) {
                    tracing::debug!(error = %e, "main-looper message handler threw (cleared, continuing)");
                }
            }

            if let Err(e) = checked(env, "Message.recycle (cached)", |env| {

                unsafe {
                    env.call_method_unchecked(
                        &msg,
                        cache.message_recycle,
                        JavaType::Primitive(Primitive::Void),
                        &[],
                    )
                }?
                .v()
            }) {
                tracing::debug!(error = %e, "Message.recycle failed (ignored)");
            }
            Ok(true)
        })?;
        if !processed {
            return Ok(main_looper_due_after_drain());
        }
    }
    Ok(MainLooperDue::Now)
}

pub fn dispatch_click_to_view(
    vm: &Vm,
    handle: view_registry::ViewHandle,
) -> Result<bool, FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| perform_click(env, handle))) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

fn perform_click(env: &mut Env, handle: view_registry::ViewHandle) -> Result<bool, FrameworkError> {
    let view = match view_registry::local_jobject(env, handle) {
        Ok(Ok(Some(view))) => view,
        Ok(Ok(None)) => return Ok(false),
        Ok(Err(e)) => return Err(FrameworkError::Jni(e)),
        Err(e) => {
            tracing::debug!(handle, error = %e, "performClick: view not dispatchable (ignored)");
            return Ok(false);
        }
    };
    checked(env, "View.performClick", |env| {
        env.call_method(&view, jni_str!("performClick"), jni_sig!("()Z"), &[])?
            .z()
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionAction {
    Down,

    Up,

    Move,
}

impl MotionAction {
    pub fn code(self) -> jint {
        match self {
            Self::Down => 0,
            Self::Up => 1,
            Self::Move => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Down,

    Up,
}

impl KeyAction {
    pub fn code(self) -> jint {
        match self {
            Self::Down => 0,
            Self::Up => 1,
        }
    }
}

pub fn dispatch_touch_to_view(
    vm: &Vm,
    handle: view_registry::ViewHandle,
    action: MotionAction,
    x: f32,
    y: f32,
) -> Result<bool, FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| touch_view(env, handle, action, x, y))) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

fn touch_view(
    env: &mut Env,
    handle: view_registry::ViewHandle,
    action: MotionAction,
    x: f32,
    y: f32,
) -> Result<bool, FrameworkError> {
    let view = match view_registry::local_jobject(env, handle) {
        Ok(Ok(Some(view))) => view,
        Ok(Ok(None)) => return Ok(false),
        Ok(Err(e)) => return Err(FrameworkError::Jni(e)),
        Err(e) => {
            tracing::debug!(handle, error = %e, "dispatchTouchEvent: view not dispatchable (ignored)");
            return Ok(false);
        }
    };
    let system_clock = env.find_class(SYSTEM_CLOCK_CLASS)?;
    let now = checked(env, "SystemClock.uptimeMillis", |env| {
        env.call_static_method(
            &system_clock,
            jni_str!("uptimeMillis"),
            jni_sig!("()J"),
            &[],
        )?
        .j()
    })?;

    let motion_event_class = env.find_class(MOTION_EVENT_CLASS)?;
    let event = checked(env, "MotionEvent.obtain", |env| {
        env.call_static_method(
            &motion_event_class,
            jni_str!("obtain"),
            jni_sig!("(JJIFFI)Landroid/view/MotionEvent;"),
            &[
                JValue::Long(now),
                JValue::Long(now),
                JValue::Int(action.code()),
                JValue::Float(x),
                JValue::Float(y),
                JValue::Int(0),
            ],
        )?
        .l()
    })?;

    let consumed = checked(env, "View.dispatchTouchEvent", |env| {
        env.call_method(
            &view,
            jni_str!("dispatchTouchEvent"),
            jni_sig!("(Landroid/view/MotionEvent;)Z"),
            &[JValue::Object(&event)],
        )?
        .z()
    });

    if let Err(e) = checked(env, "MotionEvent.recycle", |env| {
        env.call_method(&event, jni_str!("recycle"), jni_sig!("()V"), &[])?
            .v()
    }) {
        tracing::debug!(handle, error = %e, "MotionEvent.recycle failed (ignored)");
    }
    consumed
}

pub struct EngineTouchOutcome {
    pub consumed: bool,

    pub down_time_ms: i64,
}

static ENGINE_SURFACE_VIEW: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

static RETIRED_ENGINE_SURFACE_VIEW: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(0);

pub fn engine_surface_view_handle() -> Option<view_registry::ViewHandle> {
    let handle = ENGINE_SURFACE_VIEW.load(std::sync::atomic::Ordering::Acquire);
    (handle != 0).then_some(handle)
}

fn retired_engine_surface_view() -> Option<view_registry::ViewHandle> {
    let handle = RETIRED_ENGINE_SURFACE_VIEW.load(std::sync::atomic::Ordering::Acquire);
    (handle != 0).then_some(handle)
}

fn active_engine_surface_view() -> Option<view_registry::ViewHandle> {
    view_registry::find_in_subtree(view_registry::active_root(), RBX_SURFACE_VIEW_CLASS)
}

fn replacement_engine_surface_view(
    root: view_registry::ViewHandle,
    current: Option<view_registry::ViewHandle>,
    retired: Option<view_registry::ViewHandle>,
) -> Option<view_registry::ViewHandle> {
    view_registry::find_in_subtree(root, RBX_SURFACE_VIEW_CLASS)
        .filter(|&next| Some(next) != current && Some(next) != retired)
}

pub fn engine_surface_view_replaced() -> bool {
    replacement_engine_surface_view(
        view_registry::active_root(),
        engine_surface_view_handle(),
        retired_engine_surface_view(),
    )
    .is_some()
}

const NATIVE_INPUT_INTERFACE_CLASS: &JNIStr =
    jni_str!("com/roblox/engine/jni/NativeInputInterface");

struct NativeInputJniCache {
    class: Global<JClass<'static>>,
    pass_mouse_wheel: JStaticMethodID,
    pass_mouse_button: JStaticMethodID,
    pass_mouse_move: JStaticMethodID,
}

static NATIVE_INPUT_JNI_CACHE: OnceLock<NativeInputJniCache> = OnceLock::new();

static MAIN_WINDOW_IS_MOUSE_LOCKED_CENTER: OnceLock<JStaticMethodID> = OnceLock::new();

fn native_input_jni_cache(env: &mut Env) -> Result<&'static NativeInputJniCache, FrameworkError> {
    if let Some(cache) = NATIVE_INPUT_JNI_CACHE.get() {
        return Ok(cache);
    }
    let cache = checked(
        env,
        "NativeInputInterface class and static method IDs",
        |env| {
            let class = env.find_class(NATIVE_INPUT_INTERFACE_CLASS)?;
            Ok(NativeInputJniCache {
                pass_mouse_wheel: env.get_static_method_id(
                    &class,
                    jni_str!("nativePassMouseWheel"),
                    jni_sig!("(FFF)V"),
                )?,
                pass_mouse_button: env.get_static_method_id(
                    &class,
                    jni_str!("nativePassMouseButton"),
                    jni_sig!("(FFZI)V"),
                )?,
                pass_mouse_move: env.get_static_method_id(
                    &class,
                    jni_str!("nativePassMouseMove"),
                    jni_sig!("(FFFF)V"),
                )?,
                class: env.new_global_ref(&class)?,
            })
        },
    )?;
    Ok(NATIVE_INPUT_JNI_CACHE.get_or_init(|| cache))
}

fn main_window_is_mouse_locked_center_method(
    env: &mut Env,
    class: &Global<JClass<'static>>,
) -> Result<JStaticMethodID, FrameworkError> {
    if let Some(method) = MAIN_WINDOW_IS_MOUSE_LOCKED_CENTER.get() {
        return Ok(*method);
    }
    let method = checked(
        env,
        "NativeInputInterface.nativeGetMainWindowIsMouseLockedCenter static method ID",
        |env| {
            env.get_static_method_id(
                class,
                jni_str!("nativeGetMainWindowIsMouseLockedCenter"),
                jni_sig!("()Z"),
            )
        },
    )?;
    Ok(*MAIN_WINDOW_IS_MOUSE_LOCKED_CENTER.get_or_init(|| method))
}

pub fn dispatch_scroll(vm: &Vm, x: f32, y: f32, delta: f32) {
    let raw = vm.as_raw();
    if raw.is_null() {
        return;
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    let _ = java_vm.attach_current_thread(|env: &mut Env| -> Result<(), FrameworkError> {
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let Ok(cache) = native_input_jni_cache(env) else {
                return;
            };
            let args = [
                JValue::Float(x).as_jni(),
                JValue::Float(y).as_jni(),
                JValue::Float(delta).as_jni(),
            ];
            if let Err(e) = checked(env, "NativeInputInterface.nativePassMouseWheel", |env| {
                unsafe {
                    env.call_static_method_unchecked(
                        &cache.class,
                        cache.pass_mouse_wheel,
                        JavaType::Primitive(Primitive::Void),
                        &args,
                    )
                }?
                .v()
            }) {
                tracing::debug!(error = %e, "nativePassMouseWheel threw (cleared)");
            }
        }));
        Ok(())
    });
}

pub fn dispatch_mouse_button(
    vm: &Vm,
    x: f32,
    y: f32,
    down: bool,
    button: i32,
) -> Result<(), FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| {
            let cache = native_input_jni_cache(env)?;
            let args = [
                JValue::Float(x).as_jni(),
                JValue::Float(y).as_jni(),
                JValue::Bool(down).as_jni(),
                JValue::Int(button).as_jni(),
            ];
            checked(env, "NativeInputInterface.nativePassMouseButton", |env| {
                unsafe {
                    env.call_static_method_unchecked(
                        &cache.class,
                        cache.pass_mouse_button,
                        JavaType::Primitive(Primitive::Void),
                        &args,
                    )
                }?
                .v()
            })?;
            Ok(())
        })) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

pub fn dispatch_mouse_move(
    vm: &Vm,
    x: f32,
    y: f32,
    dx: f32,
    dy: f32,
) -> Result<(), FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| {
            let cache = native_input_jni_cache(env)?;
            let args = [
                JValue::Float(x).as_jni(),
                JValue::Float(y).as_jni(),
                JValue::Float(dx).as_jni(),
                JValue::Float(dy).as_jni(),
            ];
            checked(env, "NativeInputInterface.nativePassMouseMove", |env| {
                unsafe {
                    env.call_static_method_unchecked(
                        &cache.class,
                        cache.pass_mouse_move,
                        JavaType::Primitive(Primitive::Void),
                        &args,
                    )
                }?
                .v()
            })?;
            Ok(())
        })) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

pub fn engine_mouse_locked_center(vm: &Vm) -> Result<bool, FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| {
            let cache = native_input_jni_cache(env)?;
            let method = main_window_is_mouse_locked_center_method(env, &cache.class)?;
            checked(
                env,
                "NativeInputInterface.nativeGetMainWindowIsMouseLockedCenter",
                |env| {
                    unsafe {
                        env.call_static_method_unchecked(
                            &cache.class,
                            method,
                            JavaType::Primitive(Primitive::Boolean),
                            &[],
                        )
                    }?
                    .z()
                },
            )
        })) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

pub fn dispatch_touch_to_engine_surface(
    vm: &Vm,
    action: MotionAction,
    x: f32,
    y: f32,
    down_time_ms: Option<i64>,
) -> Result<Option<EngineTouchOutcome>, FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| {
            touch_engine_surface(env, action, x, y, down_time_ms)
        })) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

fn touch_engine_surface(
    env: &mut Env,
    action: MotionAction,
    x: f32,
    y: f32,
    down_time_ms: Option<i64>,
) -> Result<Option<EngineTouchOutcome>, FrameworkError> {
    let Some(handle) = engine_surface_view_handle() else {
        tracing::debug!(
            ?action,
            "engine touch: no RBXSurfaceView holds the engine surface yet (no-op)"
        );
        return Ok(None);
    };
    let surface = match view_registry::local_jobject(env, handle) {
        Ok(Ok(Some(surface))) => surface,
        Ok(Ok(None)) => return Ok(None),
        Ok(Err(e)) => return Err(FrameworkError::Jni(e)),
        Err(e) => {
            tracing::debug!(error = %e, "engine touch: surface not dispatchable (ignored)");
            return Ok(None);
        }
    };
    let system_clock = env.find_class(SYSTEM_CLOCK_CLASS)?;
    let now = checked(env, "SystemClock.uptimeMillis", |env| {
        env.call_static_method(
            &system_clock,
            jni_str!("uptimeMillis"),
            jni_sig!("()J"),
            &[],
        )?
        .j()
    })?;
    let down_time = down_time_ms.unwrap_or(now);

    if down_time_ms.is_none() {
        let w = checked(env, "View.getWidth", |env| {
            env.call_method(&surface, jni_str!("getWidth"), jni_sig!("()I"), &[])?
                .i()
        })
        .unwrap_or(-1);
        let h = checked(env, "View.getHeight", |env| {
            env.call_method(&surface, jni_str!("getHeight"), jni_sig!("()I"), &[])?
                .i()
        })
        .unwrap_or(-1);
        tracing::info!(
            handle,
            width = w,
            height = h,
            x,
            y,
            "engine touch DOWN: RBXSurfaceView resolved (geometry = coordinate-space check)"
        );
    }

    let motion_event_class = env.find_class(MOTION_EVENT_CLASS)?;
    let event = checked(env, "MotionEvent.obtain", |env| {
        env.call_static_method(
            &motion_event_class,
            jni_str!("obtain"),
            jni_sig!("(JJIFFI)Landroid/view/MotionEvent;"),
            &[
                JValue::Long(down_time),
                JValue::Long(now),
                JValue::Int(action.code()),
                JValue::Float(x),
                JValue::Float(y),
                JValue::Int(0),
            ],
        )?
        .l()
    })?;

    let consumed = checked(env, "RBXSurfaceView.onTouchEventInternal", |env| {
        env.call_method(
            &surface,
            jni_str!("onTouchEventInternal"),
            jni_sig!("(Landroid/view/MotionEvent;Z)Z"),
            &[JValue::Object(&event), JValue::Bool(false)],
        )?
        .z()
    });

    if let Err(e) = checked(env, "MotionEvent.recycle", |env| {
        env.call_method(&event, jni_str!("recycle"), jni_sig!("()V"), &[])?
            .v()
    }) {
        tracing::debug!(error = %e, "MotionEvent.recycle failed (ignored)");
    }
    Ok(Some(EngineTouchOutcome {
        consumed: consumed?,
        down_time_ms: down_time,
    }))
}

pub fn pass_hardware_key_to_engine(
    vm: &Vm,
    action: KeyAction,
    scan_code: jint,
    key_code: jint,
    is_repeat: bool,
) -> Result<(), FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| {
            let class = checked(env, "NativeGLInterface class for hardware key", |env| {
                env.find_class(jni_str!("com/roblox/engine/jni/NativeGLInterface"))
            })?;
            checked(env, "NativeGLInterface.nativePassKeyEvent", |env| {
                env.call_static_method(
                    &class,
                    jni_str!("nativePassKeyEvent"),
                    jni_sig!("(ZIIZ)V"),
                    &[
                        JValue::Bool(matches!(action, KeyAction::Down)),
                        JValue::Int(scan_code),
                        JValue::Int(key_code),
                        JValue::Bool(is_repeat),
                    ],
                )?
                .v()
            })
        })) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

const RBX_SURFACE_VIEW_CLASS: &str = "com.roblox.client.RBXSurfaceView";

const DISPLAY_CLASS: &JNIStr = jni_str!("android/view/Display");
const DISPLAY_WINDOW_WIDTH_FIELD: &JNIStr = jni_str!("window_width");
const DISPLAY_WINDOW_HEIGHT_FIELD: &JNIStr = jni_str!("window_height");

pub fn publish_window_size(vm: &Vm, width: i32, height: i32) -> Result<(), FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| -> Result<(), FrameworkError> {
            let class = checked(env, "find Display", |env| env.find_class(DISPLAY_CLASS))?;
            checked(env, "Display.window_width", |env| {
                env.set_static_field(
                    &class,
                    DISPLAY_WINDOW_WIDTH_FIELD,
                    int_field_sig(),
                    JValue::Int(width),
                )
            })?;
            checked(env, "Display.window_height", |env| {
                env.set_static_field(
                    &class,
                    DISPLAY_WINDOW_HEIGHT_FIELD,
                    int_field_sig(),
                    JValue::Int(height),
                )
            })
        })) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

pub fn publish_display_refresh_rates(
    vm: &Vm,
    current_hz: jfloat,
    supported_hz: &[jfloat],
) -> Result<(), FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| {
            let rates = JFloatArray::new(env, supported_hz.len())?;
            rates.set_region(env, 0, supported_hz)?;
            let display = checked(env, "find Display", |env| env.find_class(DISPLAY_CLASS))?;
            checked(env, "Display.setRefreshRates", |env| {
                env.call_static_method(
                    &display,
                    jni_str!("setRefreshRates"),
                    jni_sig!("(F[F)V"),
                    &[JValue::Float(current_hz), JValue::Object(&rates)],
                )?
                .v()
            })?;
            let class = checked(env, "NativeGLInterface class for display rates", |env| {
                env.find_class(jni_str!("com/roblox/engine/jni/NativeGLInterface"))
            })?;
            checked(
                env,
                "NativeGLInterface.nativePassCurrentDisplayRefreshRate",
                |env| {
                    env.call_static_method(
                        &class,
                        jni_str!("nativePassCurrentDisplayRefreshRate"),
                        jni_sig!("(F)V"),
                        &[JValue::Float(current_hz)],
                    )?
                    .v()
                },
            )?;
            checked(
                env,
                "NativeGLInterface.nativePassSupportedRefreshRates",
                |env| {
                    env.call_static_method(
                        &class,
                        jni_str!("nativePassSupportedRefreshRates"),
                        jni_sig!("([F)V"),
                        &[JValue::Object(&rates)],
                    )?
                    .v()
                },
            )?;
            Ok(())
        })) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

const WINDOW_FORMAT_RGBA_8888: jint = 1;

fn surface_callbacks<'local>(
    env: &mut Env<'local>,
    surface_view: &JObject,
) -> Result<JObject<'local>, FrameworkError> {
    let callbacks_sig = unsafe { FieldSignature::from_raw_parts(ARRAY_LIST_SIG, JavaType::Object) };
    checked(env, "SurfaceView.mCallbacks get_field", |env| {
        env.get_field(surface_view, jni_str!("mCallbacks"), &callbacks_sig)?
            .l()
    })
}

fn surface_callbacks_size(env: &mut Env, surface_view: &JObject) -> Result<jint, FrameworkError> {
    let callbacks = surface_callbacks(env, surface_view)?;
    checked(env, "SurfaceView.mCallbacks.size", |env| {
        env.call_method(&callbacks, jni_str!("size"), jni_sig!("()I"), &[])?
            .i()
    })
}

pub fn engine_surface_callback_ready(vm: &Vm) -> Result<bool, FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| surface_callback_ready(env))) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

fn surface_callback_ready(env: &mut Env) -> Result<bool, FrameworkError> {
    let Some(handle) = active_engine_surface_view() else {
        return Ok(false);
    };

    let surface_view = match view_registry::local_jobject(env, handle) {
        Ok(Ok(Some(surface_view))) => surface_view,
        Ok(Ok(None)) => return Ok(false),
        Ok(Err(e)) => return Err(FrameworkError::Jni(e)),
        Err(e) => {
            tracing::debug!(handle, error = %e, "engine_surface_callback_ready: peer not readable (retry)");
            return Ok(false);
        }
    };
    Ok(surface_callbacks_size(env, &surface_view)? > 0)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SurfaceLifecycle {
    CreatedAndChanged,
    Changed,
}

pub fn dispatch_surface_lifecycle(
    vm: &Vm,
    width: i32,
    height: i32,
) -> Result<bool, FrameworkError> {
    dispatch_surface_event(vm, SurfaceLifecycle::CreatedAndChanged, width, height)
}

pub fn dispatch_surface_changed(vm: &Vm, width: i32, height: i32) -> Result<bool, FrameworkError> {
    dispatch_surface_event(vm, SurfaceLifecycle::Changed, width, height)
}

fn dispatch_surface_event(
    vm: &Vm,
    event: SurfaceLifecycle,
    width: i32,
    height: i32,
) -> Result<bool, FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| {
            surface_lifecycle(env, event, width, height)
        })) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

fn surface_lifecycle(
    env: &mut Env,
    event: SurfaceLifecycle,
    width: i32,
    height: i32,
) -> Result<bool, FrameworkError> {
    let target = match event {
        SurfaceLifecycle::CreatedAndChanged => active_engine_surface_view(),
        SurfaceLifecycle::Changed => engine_surface_view_handle(),
    };
    let Some(handle) = target else {
        return Ok(false);
    };

    let surface_view = match view_registry::local_jobject(env, handle) {
        Ok(Ok(Some(surface_view))) => surface_view,
        Ok(Ok(None)) => return Ok(false),
        Ok(Err(e)) => return Err(FrameworkError::Jni(e)),
        Err(e) => {
            tracing::debug!(handle, error = %e, "dispatch_surface_lifecycle: peer not dispatchable (retry)");
            return Ok(false);
        }
    };

    if surface_callbacks_size(env, &surface_view)? <= 0 {
        return Ok(false);
    }

    if event == SurfaceLifecycle::CreatedAndChanged {
        checked(env, "SurfaceView.surfaceCreated", |env| {
            env.call_method(
                &surface_view,
                jni_str!("surfaceCreated"),
                jni_sig!("()V"),
                &[],
            )?
            .v()
        })?;
        ENGINE_SURFACE_VIEW.store(handle, std::sync::atomic::Ordering::Release);
    }
    checked(env, "SurfaceView.surfaceChanged", |env| {
        env.call_method(
            &surface_view,
            jni_str!("surfaceChanged"),
            jni_sig!("(III)V"),
            &[
                JValue::Int(WINDOW_FORMAT_RGBA_8888),
                JValue::Int(width),
                JValue::Int(height),
            ],
        )?
        .v()
    })?;
    Ok(true)
}

fn destroy_engine_surface(env: &mut Env) -> Result<bool, FrameworkError> {
    let Some(handle) = engine_surface_view_handle() else {
        return Ok(false);
    };
    ENGINE_SURFACE_VIEW.store(0, std::sync::atomic::Ordering::Release);
    RETIRED_ENGINE_SURFACE_VIEW.store(handle, std::sync::atomic::Ordering::Release);
    let surface_view = match view_registry::local_jobject(env, handle) {
        Ok(Ok(Some(local))) => local,
        Ok(Ok(None)) => return Ok(false),
        Ok(Err(error)) => return Err(FrameworkError::Jni(error)),
        Err(error) => {
            tracing::debug!(handle, error = %error, "surface destroy: peer not dispatchable");
            return Ok(false);
        }
    };
    let callbacks = surface_callbacks(env, &surface_view)?;
    let snapshot_object = checked(env, "SurfaceView.mCallbacks.toArray", |env| {
        env.call_method(
            &callbacks,
            jni_str!("toArray"),
            jni_sig!("()[Ljava/lang/Object;"),
            &[],
        )?
        .l()
    })?;
    let snapshot = env
        .cast_local::<JObjectArray>(snapshot_object)
        .map_err(FrameworkError::Jni)?;
    let count = snapshot.len(env).map_err(FrameworkError::Jni)?;
    if count == 0 {
        return Ok(false);
    }

    let holder_sig =
        unsafe { FieldSignature::from_raw_parts(SURFACE_HOLDER_SIG, JavaType::Object) };
    let holder = checked(env, "SurfaceView.mSurfaceHolder get_field", |env| {
        env.get_field(&surface_view, jni_str!("mSurfaceHolder"), &holder_sig)?
            .l()
    })?;

    let mut first_error = None;
    for index in 0..count {
        let callback = match snapshot.get_element(env, index) {
            Ok(callback) => callback,
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(FrameworkError::Jni(error));
                }
                continue;
            }
        };
        if let Err(error) = checked(env, "SurfaceHolder.Callback.surfaceDestroyed", |env| {
            env.call_method(
                &callback,
                jni_str!("surfaceDestroyed"),
                jni_sig!("(Landroid/view/SurfaceHolder;)V"),
                &[JValue::Object(&holder)],
            )?
            .v()
        }) {
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
    }
    if let Some(error) = first_error {
        Err(error)
    } else {
        Ok(true)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrawTarget {
    pub handle: view_registry::ViewHandle,

    pub width: u32,

    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrawnCanvas {
    pub view: view_registry::ViewHandle,

    pub canvas: canvas_registry::CanvasHandle,
}

pub fn drive_view_draw(
    vm: &Vm,
    targets: &[DrawTarget],
) -> Result<Vec<DrawnCanvas>, FrameworkError> {
    if targets.is_empty() || !canvas_draw_supported() {
        return Ok(Vec::new());
    }
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| draw_targets(env, targets))) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

fn draw_targets(env: &mut Env, targets: &[DrawTarget]) -> Result<Vec<DrawnCanvas>, FrameworkError> {
    let canvas_class = env.find_class(CANVAS_CLASS)?;
    let mut drawn = Vec::with_capacity(targets.len());
    for t in targets {
        let view = match view_registry::local_jobject(env, t.handle) {
            Ok(Ok(Some(view))) => view,
            Ok(Err(e)) => {
                tracing::debug!(view = t.handle, error = %e, "draw cascade: view local ref failed (skipped)");
                continue;
            }
            Ok(Ok(None)) | Err(_) => {
                tracing::trace!(view = t.handle, "draw cascade: view not drawable (skipped)");
                continue;
            }
        };
        let canvas_handle = match canvas_registry::allocate(t.width, t.height) {
            Ok(h) => h,
            Err(e) => {
                tracing::debug!(view = t.handle, w = t.width, h = t.height, error = %e,
                    "draw cascade: canvas allocate failed (skipped)");
                continue;
            }
        };

        let canvas_obj = match checked(env, "Canvas.<init>(long)", |env| {
            env.new_object(
                &canvas_class,
                jni_sig!("(J)V"),
                &[JValue::Long(canvas_handle)],
            )
        }) {
            Ok(o) => o,
            Err(e) => {
                tracing::debug!(view = t.handle, canvas = canvas_handle, error = %e,
                    "draw cascade: Canvas.<init> failed (skipped)");
                let _ = canvas_registry::free(canvas_handle);
                continue;
            }
        };

        let result = checked(env, "View.draw(Canvas)", |env| {
            env.call_method(
                &view,
                jni_str!("draw"),
                jni_sig!("(Landroid/graphics/Canvas;)V"),
                &[JValue::Object(&canvas_obj)],
            )?
            .v()
        });
        match result {
            Ok(()) => {
                tracing::debug!(
                    view = t.handle,
                    canvas = canvas_handle,
                    w = t.width,
                    h = t.height,
                    "draw cascade: View.draw(Canvas) ran — onDraw rasterized into the Pixmap"
                );
                drawn.push(DrawnCanvas {
                    view: t.handle,
                    canvas: canvas_handle,
                });
            }
            Err(e) => {
                tracing::debug!(view = t.handle, error = %e, "draw cascade: View.draw threw (skipped)");
                let _ = canvas_registry::free(canvas_handle);
            }
        }
    }
    Ok(drawn)
}

fn register_framework_natives(
    env: &mut Env,
    apk_path: &str,
    signing_certificate_history: &SigningCertificateHistory,
) -> Result<(), FrameworkError> {
    register_context_natives(env, apk_path)?;

    signing_certificates::register_natives(env, signing_certificate_history)?;

    preloaded_library::register_natives(env)?;

    register_log_natives(env)?;

    register_process_natives(env)?;

    register_asset_manager_natives(env)?;

    register_asset_stream_natives(env)?;

    register_xml_block_natives(env)?;

    register_environment_natives(env)?;

    captures::register_natives(env)?;

    register_system_clock_natives(env)?;

    register_message_queue_natives(env)?;

    register_sensor_manager_natives(env)?;

    register_connectivity_natives(env)?;

    register_activity_manager_memory_natives(env)?;

    register_vibrator_natives(env)?;

    sqlite::register_natives(env)?;

    register_activity_natives(env)?;

    external_intents::register_natives(env)?;

    register_view_natives(env)?;

    register_view_tree_observer_natives(env)?;

    register_input_method_manager_natives(env)?;

    register_clipboard_manager_natives(env)?;

    dialogs::register_natives(env)?;

    register_window_natives(env)?;

    keep_screen_on::register_natives(env)?;

    notifications::register_natives(env)?;

    register_text_view_natives(env)?;

    register_image_view_natives(env)?;

    register_image_button_natives(env)?;

    register_surface_view_natives(env)?;

    register_view_subclass_constructor_natives(env)?;

    register_web_view_natives(env)?;

    register_web_settings_natives(env)?;

    register_cookie_manager_natives(env)?;

    register_widget_property_setter_natives(env)?;

    register_drawable_natives(env)?;

    register_bitmap_natives(env)?;

    register_view_group_natives(env)?;

    register_paint_natives(env)?;

    register_matrix_natives(env)?;

    register_path_natives(env)?;

    register_canvas_natives(env)?;

    register_runtime_native_load_natives(env)?;
    Ok(())
}

fn drive_lifecycle(
    env: &mut Env,
    apk_path: &str,
    native_library_dir: &str,
    signing_certificate_history: &SigningCertificateHistory,
    start: ActivityStart<'_>,
) -> Result<LifecycleProgress, FrameworkError> {
    register_framework_natives(env, apk_path, signing_certificate_history)?;

    env.find_class(CONTEXT_CLASS)?;
    env.find_class(APPLICATION_CLASS)?;
    tracing::info!(
        context = STEP1_CREATE_APPLICATION.class,
        application = STEP3_APPLICATION_ON_CREATE.class,
        "framework bridge proven: Context static-init natives registered + bootstrap classes resolved via JNI"
    );

    prepare_main_looper_inner(env)?;

    let window_handle = window_registry::allocate()?;
    let context = env.find_class(CONTEXT_CLASS)?;
    publish_native_library_dir(env, &context, native_library_dir)?;
    let app = checked(env, "step 1 Context.createApplication", |env| {
        env.call_static_method(
            &context,
            jni_str!("createApplication"),
            jni_sig!("(J)Landroid/app/Application;"),
            &[JValue::Long(window_handle)],
        )?
        .l()
    })?;

    let content_provider = env.find_class(CONTENT_PROVIDER_CLASS)?;
    checked(
        env,
        "step 2 ContentProvider.createContentProviders",
        |env| {
            env.call_static_method(
                &content_provider,
                jni_str!("createContentProviders"),
                jni_sig!("()V"),
                &[],
            )?
            .v()
        },
    )?;

    checked(env, "step 3 Application.onCreate", |env| {
        env.call_method(&app, jni_str!("onCreate"), jni_sig!("()V"), &[])?
            .v()
    })?;
    tracing::info!("Application.onCreate reached: recipe steps 1–3 driven");

    let activity_class = env.find_class(ACTIVITY_CLASS)?;
    let activity_name = start.activity();
    let class_name_jstr = env.new_string(activity_name)?;
    let activity_uri_jstr = start.uri().map(|uri| env.new_string(uri)).transpose()?;
    let null_activity_uri = JObject::null();
    let activity_uri_object: &JObject = activity_uri_jstr
        .as_ref()
        .map(AsRef::as_ref)
        .unwrap_or(&null_activity_uri);
    let activity = checked(env, "step 4 Activity.createMainActivity", |env| {
        env.call_static_method(
            &activity_class,
            jni_str!("createMainActivity"),
            jni_sig!("(Ljava/lang/String;JLjava/lang/String;)Landroid/app/Activity;"),
            &[
                JValue::Object(&class_name_jstr),
                JValue::Long(window_handle),
                JValue::Object(activity_uri_object),
            ],
        )?
        .l()
    })?;

    track_activity(env, &activity);

    call_activity_on_create(env, &activity, "step 5 Activity.onCreate")?;
    tracing::info!(
        activity = activity_name,
        "Activity.onCreate reached: recipe steps 1–5 driven (launcher Activity onCreate)"
    );

    call_activity_on_post_create(env, &activity, "step 5b Activity.onPostCreate")?;

    call_activity_on_start(env, &activity, "step 6 Activity.onStart")?;

    call_activity_on_resume(env, &activity, "step 7 Activity.onResume")?;
    tracing::info!(
        activity = activity_name,
        "Activity resumed: recipe steps 1–7 driven (launcher Activity onStart + onResume)"
    );
    Ok(LifecycleProgress::ActivityResumed)
}

const ATL_LOADED_APP_CLASS: &JNIStr = jni_str!("android/atl/ATLLoadedApp");
const PACKAGE_MANAGER_SIG: &JNIStr = jni_str!("Landroid/content/pm/PackageManager;");
const PARSED_PACKAGE_SIG: &JNIStr = jni_str!("Landroid/content/pm/PackageParser$Package;");
const APPLICATION_INFO_SIG: &JNIStr = jni_str!("Landroid/content/pm/ApplicationInfo;");
const STRING_SIG: &JNIStr = jni_str!("Ljava/lang/String;");

fn publish_native_library_dir(
    env: &mut Env,
    context: &JClass,
    native_library_dir: &str,
) -> Result<(), FrameworkError> {
    let object_sig = |sig| unsafe { FieldSignature::from_raw_parts(sig, JavaType::Object) };
    checked(env, "Context.<clinit> via Context.package_manager", |env| {
        env.get_static_field(
            context,
            jni_str!("package_manager"),
            object_sig(PACKAGE_MANAGER_SIG),
        )
        .map(drop)
    })?;
    let loaded_app_class = env.find_class(ATL_LOADED_APP_CLASS)?;
    let application_info = checked(env, "primary ApplicationInfo", |env| {
        let loaded_app = env
            .call_static_method(
                &loaded_app_class,
                jni_str!("getPrimaryApplication"),
                jni_sig!("()Landroid/atl/ATLLoadedApp;"),
                &[],
            )?
            .l()?;
        let package = env
            .get_field(&loaded_app, jni_str!("pkg"), object_sig(PARSED_PACKAGE_SIG))?
            .l()?;
        env.get_field(
            &package,
            jni_str!("applicationInfo"),
            object_sig(APPLICATION_INFO_SIG),
        )?
        .l()
    })?;
    let dir = env.new_string(native_library_dir)?;
    checked(env, "ApplicationInfo.nativeLibraryDir", |env| {
        env.set_field(
            &application_info,
            jni_str!("nativeLibraryDir"),
            object_sig(STRING_SIG),
            JValue::Object(&dir),
        )
    })
}

fn checked<'local, T>(
    env: &mut Env<'local>,
    what: &str,
    op: impl FnOnce(&mut Env<'local>) -> Result<T, jni::errors::Error>,
) -> Result<T, FrameworkError> {
    match op(env) {
        Ok(value) => Ok(value),
        Err(e) => {
            if env.exception_check() {
                env.exception_describe();
                env.exception_clear();
            }
            tracing::error!(step = what, error = %e, "framework lifecycle step failed");
            Err(FrameworkError::Jni(e))
        }
    }
}

fn call_activity_on_create<'local>(
    env: &mut Env<'local>,
    activity: &JObject,
    what: &str,
) -> Result<(), FrameworkError> {
    checked(env, what, |env| {
        env.call_method(
            activity,
            jni_str!("onCreate"),
            jni_sig!("(Landroid/os/Bundle;)V"),
            &[JValue::Object(&JObject::null())],
        )?
        .v()
    })
}

fn call_activity_on_post_create<'local>(
    env: &mut Env<'local>,
    activity: &JObject,
    what: &str,
) -> Result<(), FrameworkError> {
    checked(env, what, |env| {
        env.call_method(
            activity,
            jni_str!("onPostCreate"),
            jni_sig!("(Landroid/os/Bundle;)V"),
            &[JValue::Object(&JObject::null())],
        )?
        .v()
    })
}

fn call_activity_on_start<'local>(
    env: &mut Env<'local>,
    activity: &JObject,
    what: &str,
) -> Result<(), FrameworkError> {
    checked(env, what, |env| {
        env.call_method(activity, jni_str!("onStart"), jni_sig!("()V"), &[])?
            .v()
    })
}

fn call_activity_on_resume<'local>(
    env: &mut Env<'local>,
    activity: &JObject,
    what: &str,
) -> Result<(), FrameworkError> {
    checked(env, what, |env| {
        env.call_method(activity, jni_str!("onResume"), jni_sig!("()V"), &[])?
            .v()
    })
}

fn drive_activity_down_lifecycle<'local>(
    env: &mut Env<'local>,
    activity: &JObject,
) -> Result<(), FrameworkError> {
    let steps = [
        call_activity_on_pause(env, activity, "nativeFinish Activity.onPause"),
        destroy_engine_surface_of(env, activity),
        call_activity_on_stop(env, activity, "nativeFinish Activity.onStop"),
        call_activity_on_destroy(env, activity, "nativeFinish Activity.onDestroy"),
    ];
    steps.into_iter().collect()
}

fn destroy_engine_surface_of(env: &mut Env, activity: &JObject) -> Result<(), FrameworkError> {
    if activity_hosts_engine_surface(env, activity)? {
        destroy_engine_surface(env)?;
    }
    Ok(())
}

fn activity_hosts_engine_surface(
    env: &mut Env,
    activity: &JObject,
) -> Result<bool, FrameworkError> {
    let Some(surface_view) = engine_surface_view_handle() else {
        return Ok(false);
    };
    Ok(activity_content_root(env, activity)?
        .is_some_and(|root| view_registry::subtree_contains(root, surface_view)))
}

fn activity_content_root(
    env: &mut Env,
    activity: &JObject,
) -> Result<Option<view_registry::ViewHandle>, FrameworkError> {
    let window = checked(env, "nativeFinish Activity.getWindow", |env| {
        env.call_method(
            activity,
            jni_str!("getWindow"),
            jni_sig!("()Landroid/view/Window;"),
            &[],
        )?
        .l()
    })?;
    if window.is_null() {
        return Ok(None);
    }
    let decor = checked(env, "nativeFinish Window.getDecorView", |env| {
        env.call_method(
            &window,
            jni_str!("getDecorView"),
            jni_sig!("()Landroid/view/View;"),
            &[],
        )?
        .l()
    })?;
    if decor.is_null() {
        return Ok(None);
    }
    let widget_sig = unsafe {
        FieldSignature::from_raw_parts(VIEW_WIDGET_FIELD_SIG, JavaType::Primitive(Primitive::Long))
    };
    let decor_widget = checked(env, "nativeFinish View.widget", |env| {
        env.get_field(&decor, VIEW_WIDGET_FIELD_NAME, &widget_sig)?
            .j()
    })?;
    Ok(Some(decor_widget))
}

fn call_activity_on_pause<'local>(
    env: &mut Env<'local>,
    activity: &JObject,
    what: &str,
) -> Result<(), FrameworkError> {
    checked(env, what, |env| {
        env.call_method(activity, jni_str!("onPause"), jni_sig!("()V"), &[])?
            .v()
    })
}

fn call_activity_on_stop<'local>(
    env: &mut Env<'local>,
    activity: &JObject,
    what: &str,
) -> Result<(), FrameworkError> {
    checked(env, what, |env| {
        env.call_method(activity, jni_str!("onStop"), jni_sig!("()V"), &[])?
            .v()
    })
}

fn call_activity_on_destroy<'local>(
    env: &mut Env<'local>,
    activity: &JObject,
    what: &str,
) -> Result<(), FrameworkError> {
    checked(env, what, |env| {
        env.call_method(activity, jni_str!("onDestroy"), jni_sig!("()V"), &[])?
            .v()
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostShutdownStep {
    PauseActivities,
    DestroyEngineSurface,
    StopActivities,
    DestroyActivities,
}

const HOST_SHUTDOWN_SEQUENCE: [HostShutdownStep; 4] = [
    HostShutdownStep::PauseActivities,
    HostShutdownStep::DestroyEngineSurface,
    HostShutdownStep::StopActivities,
    HostShutdownStep::DestroyActivities,
];

pub fn drive_application_shutdown_lifecycle(vm: &Vm) -> Result<(), FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| drive_application_shutdown_inner(env))) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

fn drive_application_shutdown_inner(env: &mut Env) -> Result<(), FrameworkError> {
    let (activities, mut first_error) = snapshot_live_activities_for_shutdown(env);
    tracing::info!(
        activities = activities.len(),
        "host shutdown: driving Android lifecycle before the host window is destroyed"
    );
    let mut surface_destroyed = false;

    for step in HOST_SHUTDOWN_SEQUENCE {
        match step {
            HostShutdownStep::PauseActivities => {
                for activity in &activities {
                    remember_shutdown_error(
                        &mut first_error,
                        set_activity_finishing(env, activity),
                    );
                    remember_shutdown_error(
                        &mut first_error,
                        call_activity_on_pause(env, activity, "host shutdown Activity.onPause"),
                    );
                }
            }
            HostShutdownStep::DestroyEngineSurface => match destroy_engine_surface(env) {
                Ok(dispatched) => surface_destroyed = dispatched,
                Err(error) => remember_shutdown_error(&mut first_error, Err(error)),
            },
            HostShutdownStep::StopActivities => {
                for activity in &activities {
                    remember_shutdown_error(
                        &mut first_error,
                        call_activity_on_stop(env, activity, "host shutdown Activity.onStop"),
                    );
                }
            }
            HostShutdownStep::DestroyActivities => {
                for activity in &activities {
                    remember_shutdown_error(
                        &mut first_error,
                        call_activity_on_destroy(env, activity, "host shutdown Activity.onDestroy"),
                    );
                }
            }
        }
    }

    tracing::info!(
        activities = activities.len(),
        surface_destroyed,
        "host shutdown: Android activity/surface lifecycle completed before window teardown"
    );
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn snapshot_live_activities_for_shutdown<'local>(
    env: &mut Env<'local>,
) -> (Vec<JObject<'local>>, Option<FrameworkError>) {
    let mut tracker = match TRACKED_ACTIVITIES.lock() {
        Ok(tracker) => tracker,
        Err(error) => {
            tracing::warn!(
                target: "android.app.Activity",
                error = %error,
                "host shutdown: activity tracker poisoned; surface teardown will still run"
            );
            return (Vec::new(), Some(FrameworkError::ActivityTrackerPoisoned));
        }
    };
    let mut activities = Vec::new();
    let mut first_error = None;
    for entry in tracker.iter_mut().rev() {
        let Some(live) = entry.live() else {
            continue;
        };
        let activity = match checked(env, "host shutdown Activity NewLocalRef", |env| {
            env.new_local_ref(live.as_obj())
        }) {
            Ok(activity) => activity,
            Err(error) => {
                remember_shutdown_error(&mut first_error, Err(error));
                continue;
            }
        };
        match checked(env, "host shutdown Activity NewWeakGlobalRef", |env| {
            env.new_weak_ref(&activity)
        }) {
            Ok(finished) => *entry = TrackedActivity::Finished(finished),
            Err(error) => remember_shutdown_error(&mut first_error, Err(error)),
        }
        activities.push(activity);
    }
    (activities, first_error)
}

fn set_activity_finishing(env: &mut Env, activity: &JObject) -> Result<(), FrameworkError> {
    let boolean_sig = unsafe {
        FieldSignature::from_raw_parts(BOOLEAN_FIELD_SIG, JavaType::Primitive(Primitive::Boolean))
    };
    checked(env, "host shutdown Activity.finishing", |env| {
        env.set_field(
            activity,
            ACTIVITY_FINISHING_FIELD_NAME,
            &boolean_sig,
            JValue::Bool(true),
        )
    })
}

fn remember_shutdown_error(
    first_error: &mut Option<FrameworkError>,
    result: Result<(), FrameworkError>,
) {
    if first_error.is_none() {
        if let Err(error) = result {
            *first_error = Some(error);
        }
    }
}

#[derive(Debug)]
pub enum FrameworkError {
    NullVm,

    ActivityTrackerPoisoned,

    GlobalLayoutObserverRegistryPoisoned,

    Jni(jni::errors::Error),

    Panicked,

    WindowRegistry(window_registry::WindowRegistryError),

    ViewRegistry(view_registry::ViewRegistryError),

    SigningCertificateHistoryConflict,

    OverlayPredatesSigningCertificates(jni::errors::Error),

    OverlayPredatesPreloadedLibraries(jni::errors::Error),

    NativeLibraryDirNotUtf8(std::path::PathBuf),

    MainThreadLooper(std::io::Error),

    DialogNotShowing(window_registry::WindowHandle),

    DialogHasNoItems(window_registry::WindowHandle),

    DialogItemOutOfRange(usize),

    DialogViewGone(view_registry::ViewHandle),
}

impl fmt::Display for FrameworkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NullVm => f.write_str("framework driver received a null JavaVM pointer"),
            Self::ActivityTrackerPoisoned => {
                f.write_str("the framework Activity tracker was poisoned")
            }
            Self::GlobalLayoutObserverRegistryPoisoned => {
                f.write_str("the framework global-layout observer registry was poisoned")
            }
            Self::Jni(e) => write!(f, "JNI error driving the framework lifecycle: {e}"),
            Self::Panicked => {
                f.write_str("a panic was caught at the framework JNI boundary (not propagated)")
            }
            Self::WindowRegistry(e) => write!(f, "window-registry handle allocation failed: {e}"),
            Self::ViewRegistry(e) => write!(f, "view-registry operation failed: {e}"),
            Self::SigningCertificateHistoryConflict => {
                f.write_str("a different signing certificate history is already registered")
            }
            Self::OverlayPredatesSigningCertificates(e) => write!(
                f,
                "the Android framework overlay predates host-verified signing certificates \
                 ({e}); rebuild it with tools/framework-overlay/patch-framework.sh"
            ),
            Self::OverlayPredatesPreloadedLibraries(e) => write!(
                f,
                "the Android framework overlay predates deferred native library \
                 initialization ({e}); rebuild it with tools/framework-overlay/patch-framework.sh"
            ),
            Self::NativeLibraryDirNotUtf8(dir) => write!(
                f,
                "the native library directory {} is not valid UTF-8, so \
                 ApplicationInfo.nativeLibraryDir cannot name it; set ECLIPSE_NATIVE_LIB_DIR \
                 to a UTF-8 path",
                dir.display()
            ),
            Self::MainThreadLooper(e) => {
                write!(
                    f,
                    "creating the Android main thread's native ALooper failed: {e}"
                )
            }
            Self::DialogNotShowing(dialog) => write!(f, "dialog {dialog} is not showing"),
            Self::DialogHasNoItems(dialog) => write!(f, "dialog {dialog} has no item list"),
            Self::DialogItemOutOfRange(index) => {
                write!(f, "dialog item {index} does not fit a Java int")
            }
            Self::DialogViewGone(view) => {
                write!(f, "the dialog view {view} no longer has a Java object")
            }
        }
    }
}

impl std::error::Error for FrameworkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Jni(e)
            | Self::OverlayPredatesSigningCertificates(e)
            | Self::OverlayPredatesPreloadedLibraries(e) => Some(e),
            Self::WindowRegistry(e) => Some(e),
            Self::ViewRegistry(e) => Some(e),
            Self::MainThreadLooper(e) => Some(e),
            Self::NullVm
            | Self::ActivityTrackerPoisoned
            | Self::GlobalLayoutObserverRegistryPoisoned
            | Self::Panicked
            | Self::SigningCertificateHistoryConflict
            | Self::NativeLibraryDirNotUtf8(_)
            | Self::DialogNotShowing(_)
            | Self::DialogHasNoItems(_)
            | Self::DialogItemOutOfRange(_)
            | Self::DialogViewGone(_) => None,
        }
    }
}

impl From<window_registry::WindowRegistryError> for FrameworkError {
    fn from(e: window_registry::WindowRegistryError) -> Self {
        Self::WindowRegistry(e)
    }
}

impl From<jni::errors::Error> for FrameworkError {
    fn from(e: jni::errors::Error) -> Self {
        Self::Jni(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEXTBOX_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn a_link_launch_passes_the_resolved_activity_together_with_the_uri() {
        let launcher = "com.roblox.client.startup.ActivitySplash";
        let activity = "com.roblox.client.ActivityProtocolLaunch";
        let uri = "roblox://placeId=90441122676618";

        let plain = ActivityStart::Launcher(launcher);
        assert_eq!((plain.activity(), plain.uri()), (launcher, None));
        let link = ActivityStart::View { activity, uri };
        assert_eq!((link.activity(), link.uri()), (activity, Some(uri)));
    }

    #[test]
    fn host_shutdown_destroys_engine_surface_before_window_teardown() {
        assert_eq!(
            HOST_SHUTDOWN_SEQUENCE,
            [
                HostShutdownStep::PauseActivities,
                HostShutdownStep::DestroyEngineSurface,
                HostShutdownStep::StopActivities,
                HostShutdownStep::DestroyActivities,
            ]
        );
    }

    #[test]
    fn record_textbox_session_invalidates_geometry_and_input_type_together() {
        let _textbox_test_guard = TEXTBOX_TEST_LOCK.lock().expect("textbox test lock");
        record_textbox_session(Some(TextboxSession {
            widget: 41,
            geometry: (10, 20, 300, 40),
            input_type: 5,
            font: 39,
            font_size: 18.0,
            multiline: false,
            text_wrapped: false,
            text_color: -1,
            x_alignment: 0,
            y_alignment: 1,
            editable: true,
        }));
        assert_eq!(textbox_geometry(), Some((10, 20, 300, 40)));
        assert_eq!(textbox_input_type(), 5);

        record_textbox_session(None);
        assert_eq!(textbox_geometry(), None);
        assert_eq!(
            textbox_input_type(),
            i32::MIN,
            "the mask must not outlive the session that justified it"
        );

        record_textbox_session(Some(TextboxSession {
            widget: 42,
            geometry: (181, 149, 438, 46),
            input_type: 7,
            font: 39,
            font_size: 18.0,
            multiline: false,
            text_wrapped: false,
            text_color: -1,
            x_alignment: 0,
            y_alignment: 1,
            editable: true,
        }));
        assert_eq!(textbox_input_type(), 7);
        record_textbox_session(None);
        assert_eq!(textbox_geometry(), None);
        assert_eq!(textbox_input_type(), i32::MIN);

        let username = TextboxSession {
            widget: 42,
            geometry: (181, 149, 438, 46),
            input_type: 7,
            font: 39,
            font_size: 18.0,
            multiline: false,
            text_wrapped: false,
            text_color: -1,
            x_alignment: 0,
            y_alignment: 1,
            editable: true,
        };
        assert!(textbox_session_matches_active(username, 42));
        assert!(!textbox_session_matches_active(username, 43));
        assert!(!textbox_session_matches_active(username, 0));

        ACTIVE_TEXT_FIELD.store(42, std::sync::atomic::Ordering::Release);
        record_textbox_session(Some(username));
        assert!(has_live_textbox_session(42));
        assert!(clear_active_text_field());
        assert_eq!(active_text_field(), 0);
        assert!(!has_live_textbox_session(42));

        let password = TextboxSession {
            widget: 43,
            geometry: (181, 219, 438, 46),
            input_type: 5,
            font: 39,
            font_size: 18.0,
            multiline: false,
            text_wrapped: false,
            text_color: -1,
            x_alignment: 0,
            y_alignment: 1,
            editable: true,
        };
        ACTIVE_TEXT_FIELD.store(43, std::sync::atomic::Ordering::Release);
        record_textbox_session(Some(password));
        clear_active_text_field_if(42);
        assert_eq!(active_text_field(), 43);
        assert!(has_live_textbox_session(43));
        assert!(clear_active_text_field());
    }

    fn forget_text_pointer_press() {
        *PENDING_TEXT_POINTER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    #[test]
    fn a_press_requeries_the_text_box_while_its_text_stays_drawn() {
        let _textbox_test_guard = TEXTBOX_TEST_LOCK.lock().expect("textbox test lock");
        let widget = view_registry::allocate("android.widget.EditText").expect("view");
        ACTIVE_TEXT_FIELD.store(widget, std::sync::atomic::Ordering::Release);
        record_textbox_session(Some(chat_bar_session(widget)));
        record_widget_text(widget, Some("hello".to_owned())).expect("set text");

        assert!(prepare_text_field_pointer_press((900.0, 900.0)));

        assert_eq!(active_text_field(), widget);
        assert!(textbox_geometry_pending());
        assert_eq!(
            active_text_overlay().map(|overlay| overlay.text),
            Some("hello".to_owned())
        );
        forget_text_pointer_press();
        assert!(clear_active_text_field());
        view_registry::free(widget).expect("free view");
    }

    #[test]
    fn the_overlay_never_pairs_new_text_with_the_previous_cursor() {
        let _textbox_test_guard = TEXTBOX_TEST_LOCK.lock().expect("textbox test lock");
        let widget = view_registry::allocate("android.widget.EditText").expect("view");
        ACTIVE_TEXT_FIELD.store(widget, std::sync::atomic::Ordering::Release);
        record_textbox_session(Some(TextboxSession {
            widget,
            geometry: (0, 0, 300, 40),
            input_type: 0,
            font: 46,
            font_size: 18.0,
            multiline: false,
            text_wrapped: false,
            text_color: -1,
            x_alignment: 0,
            y_alignment: 0,
            editable: true,
        }));
        let texts = ["a".repeat(20_000), "\u{E9}".repeat(40_000)];
        let written = std::sync::atomic::AtomicBool::new(false);
        let mut snapshots = 0;
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for round in 0..200 {
                    record_widget_text(widget, Some(texts[round % 2].clone())).expect("set text");
                }
                written.store(true, std::sync::atomic::Ordering::Release);
            });
            while !written.load(std::sync::atomic::Ordering::Acquire) {
                let Some(overlay) = active_text_overlay() else {
                    continue;
                };
                snapshots += 1;
                assert_eq!(
                    overlay.selection,
                    TextSelection::Cursor(overlay.text.encode_utf16().count()),
                    "snapshot {snapshots} pairs the text with another text's cursor"
                );
            }
        });
        assert!(clear_active_text_field());
        view_registry::free(widget).expect("free view");
    }

    fn compose(text: &str) -> TextEdit<'_> {
        TextEdit::Compose { text, cursor: None }
    }

    fn focus_composing_field(preedit: &str) -> i64 {
        let widget = view_registry::allocate("android.widget.EditText").expect("view");
        ACTIVE_TEXT_FIELD.store(widget, std::sync::atomic::Ordering::Release);
        record_textbox_session(Some(chat_bar_session(widget)));
        let composing = apply_text_edit(field("", 0), compose(preedit), SINGLE_LINE);
        view_registry::with_view(widget, |view| view.text = Some(composing.text.clone()))
            .expect("set text");
        store_text_field_state(widget, &composing);
        take_host_ime_reset();
        widget
    }

    #[test]
    fn a_press_during_ime_composition_keeps_the_preedit_where_the_ime_has_it() {
        let _textbox_test_guard = TEXTBOX_TEST_LOCK.lock().expect("textbox test lock");
        let widget = focus_composing_field("にほん");

        assert!(prepare_text_field_pointer_press((182.0, 178.0)));
        assert_eq!(active_cursor_utf16(), 3);
        let text = view_registry::with_view(widget, |view| view.text.clone().unwrap_or_default())
            .expect("text");
        let committed = apply_all(
            load_text_field_state(widget, text),
            &[compose("にほんご"), compose(""), TextEdit::Type("日本語")],
            SINGLE_LINE,
        );

        assert_eq!(committed.text, "日本語");
        assert!(!take_host_ime_reset());
        forget_text_pointer_press();
        assert!(clear_active_text_field());
        view_registry::free(widget).expect("free view");
    }

    #[test]
    fn engine_text_that_rewrites_the_preedit_resets_the_host_ime() {
        let _textbox_test_guard = TEXTBOX_TEST_LOCK.lock().expect("textbox test lock");
        let widget = focus_composing_field("abc");

        record_widget_text(widget, Some("abc!".to_owned())).expect("set text");
        assert!(!take_host_ime_reset());
        assert_eq!(
            load_text_field_state(widget, "abc!".to_owned()).composing,
            Some(0..3)
        );

        record_widget_text(widget, Some("ABC!".to_owned())).expect("set text");
        assert!(take_host_ime_reset());
        assert_eq!(
            load_text_field_state(widget, "ABC!".to_owned()).composing,
            None
        );
        assert!(clear_active_text_field());
        assert!(!take_host_ime_reset());
        view_registry::free(widget).expect("free view");

        let widget = focus_composing_field("abc");
        assert!(clear_active_text_field());
        assert!(take_host_ime_reset());
        view_registry::free(widget).expect("free view");
    }

    #[test]
    fn a_press_on_another_text_box_places_the_caret_once_that_box_takes_focus() {
        if crate::host_fonts::system_font().is_none() {
            eprintln!("SKIP: no host font for the text layout");
            return;
        }
        let _textbox_test_guard = TEXTBOX_TEST_LOCK.lock().expect("textbox test lock");
        let widget = view_registry::allocate("android.widget.EditText").expect("view");
        let focused = TextboxSession {
            geometry: (100, 50, 400, 40),
            ..chat_bar_session(widget)
        };
        let clicked = TextboxSession {
            geometry: (100, 300, 400, 40),
            ..chat_bar_session(widget)
        };
        ACTIVE_TEXT_FIELD.store(widget, std::sync::atomic::Ordering::Release);
        record_textbox_session(Some(focused));
        record_widget_text(widget, Some("chat".to_owned())).expect("set text");
        let press = (125.0, 320.0);

        assert!(prepare_text_field_pointer_press(press));
        record_textbox_session(Some(focused));
        assert!(clear_active_text_field());
        record_widget_text(widget, Some("Roblox".to_owned())).expect("set text");
        ACTIVE_TEXT_FIELD.store(widget, std::sync::atomic::Ordering::Release);
        ACTIVE_TEXT_CURSOR_UTF16.store(6, std::sync::atomic::Ordering::Release);
        record_textbox_session(Some(clicked));

        let expected = text_cursor_from_pointer("Roblox", 6, clicked, press).expect("inside");
        assert!(expected < 6);
        assert_eq!(active_cursor_utf16(), expected);
        assert!(PENDING_TEXT_POINTER.lock().expect("pointer").is_none());
        assert!(clear_active_text_field());
        view_registry::free(widget).expect("free view");
    }

    #[test]
    fn the_apk_reads_and_moves_the_caret_the_overlay_draws() {
        let _textbox_test_guard = TEXTBOX_TEST_LOCK.lock().expect("textbox test lock");
        let widget = view_registry::allocate("android.widget.EditText").expect("view");
        let selection = |edge| text_field_selection(widget, edge).expect("selection");
        ACTIVE_TEXT_FIELD.store(widget, std::sync::atomic::Ordering::Release);
        record_textbox_session(Some(chat_bar_session(widget)));
        record_widget_text(widget, Some("h\u{1F600}llo".to_owned())).expect("set text");
        assert_eq!(selection(SelectionEdge::Start), 6);
        assert_eq!(selection(SelectionEdge::End), 6);

        ACTIVE_TEXT_CURSOR_UTF16.store(3, std::sync::atomic::Ordering::Release);
        assert_eq!(selection(SelectionEdge::Start), 3);
        assert!(select_all_active_text_field());
        assert_eq!(selection(SelectionEdge::Start), 0);
        assert_eq!(selection(SelectionEdge::End), 6);

        set_text_field_selection(widget, 4).expect("set selection");
        assert_eq!(
            active_text_overlay().map(|overlay| overlay.selection),
            Some(TextSelection::Cursor(4))
        );
        set_text_field_selection(widget, 2).expect("set selection");
        assert_eq!(selection(SelectionEdge::End), 1);
        set_text_field_selection(widget, 99).expect("set selection");
        assert_eq!(selection(SelectionEdge::Start), 6);

        assert!(clear_active_text_field());
        set_text_field_selection(widget, 1).expect("set selection");
        assert_eq!(active_cursor_utf16(), 0);
        assert_eq!(selection(SelectionEdge::Start), 6);
        view_registry::free(widget).expect("free view");
    }

    #[test]
    fn selection_natives_match_the_edit_text_overlay_declarations() {
        let script = include_str!("../tools/framework-overlay/patch-framework.sh");
        for (name, signature) in [
            (
                EDIT_TEXT_GET_SELECTION_START_NAME,
                EDIT_TEXT_GET_SELECTION_SIG,
            ),
            (
                EDIT_TEXT_GET_SELECTION_END_NAME,
                EDIT_TEXT_GET_SELECTION_SIG,
            ),
            (EDIT_TEXT_SET_SELECTION_NAME, EDIT_TEXT_SET_SELECTION_SIG),
        ] {
            let declaration = format!(
                ".method private native {}{}",
                name.to_str(),
                signature.to_str()
            );
            assert!(script.contains(&declaration), "{declaration}");
        }
    }

    #[test]
    fn new_engine_text_collapses_a_select_all_to_a_caret_at_its_end() {
        let _textbox_test_guard = TEXTBOX_TEST_LOCK.lock().expect("textbox test lock");
        let widget = view_registry::allocate("android.widget.EditText").expect("view");
        ACTIVE_TEXT_FIELD.store(widget, std::sync::atomic::Ordering::Release);
        record_textbox_session(Some(chat_bar_session(widget)));
        record_widget_text(widget, Some("abc".to_owned())).expect("set text");
        assert!(select_all_active_text_field());

        record_widget_text(widget, Some("hello world".to_owned())).expect("set text");

        assert_eq!(
            active_text_overlay().map(|overlay| overlay.selection),
            Some(TextSelection::Cursor(11))
        );
        let state = load_text_field_state(widget, "hello world".to_owned());
        assert_eq!(insert_text(state, "x").text, "hello worldx");
        assert!(clear_active_text_field());
        view_registry::free(widget).expect("free view");
    }

    #[test]
    fn password_shown_input_types_render_in_clear() {
        for shown in [0, 1, 2, 3, 4, 6, 7, 8, 10] {
            assert_eq!(
                displayed_text_box_text("Ab1!", shown),
                "Ab1!",
                "input type {shown}"
            );
        }
        for secret in [5, 9, 11, -1, i32::MIN] {
            assert_eq!(
                displayed_text_box_text("Ab1!", secret),
                "\u{2022}\u{2022}\u{2022}\u{2022}",
                "input type {secret}"
            );
        }
        assert_eq!(displayed_text_box_text("", 5), "");
    }

    #[test]
    fn clicks_in_any_focused_text_box_use_the_layout_the_overlay_draws() {
        if crate::host_fonts::system_font().is_none() {
            eprintln!("SKIP: no host font for the text layout");
            return;
        }
        let session = TextboxSession {
            widget: 46,
            geometry: (100, 50, 400, 40),
            input_type: 0,
            font: 46,
            font_size: 20.0,
            multiline: false,
            text_wrapped: false,
            text_color: -1,
            x_alignment: 0,
            y_alignment: 1,
            editable: true,
        };
        let text = "hello world";
        let end = text.encode_utf16().count();
        assert_eq!(
            text_cursor_from_pointer(text, end, session, (101.0, 70.0)),
            Some(0)
        );
        assert_eq!(
            text_cursor_from_pointer(text, end, session, (499.0, 70.0)),
            jint::try_from(end).ok()
        );
        let middle = text_cursor_from_pointer(text, end, session, (140.0, 70.0))
            .expect("a click inside the text lands on it");
        assert!(0 < middle && (middle as usize) < end);
        assert_eq!(
            text_cursor_from_pointer(text, end, session, (99.0, 70.0)),
            None
        );
    }

    #[test]
    fn native_ui_text_writers_keep_raw_text_out_of_logs() {
        let src = include_str!("framework.rs");
        for marker in [
            "extern \"system\" fn text_view_native_set_text",
            "extern \"system\" fn widget_native_set_text",
            "extern \"system\" fn radio_button_set_text",
        ] {
            let start = src
                .find(marker)
                .unwrap_or_else(|| panic!("missing {marker}"));
            let rest = &src[start..];
            let end = rest.find("\n///").unwrap_or(rest.len());
            let body = &rest[..end];
            assert!(
                body.contains("chars = value.as_deref().map_or"),
                "{marker} must log only the character count"
            );
            assert!(
                !body.contains("\n                text =")
                    && !body.contains("\n                value ="),
                "{marker} must never bind raw UI text to a log field"
            );
        }
    }

    #[test]
    fn recipe_descriptors_match_confirmed_spec() {
        assert_eq!(STEP1_CREATE_APPLICATION.class, "android/content/Context");
        assert_eq!(STEP1_CREATE_APPLICATION.method, "createApplication");
        assert_eq!(
            STEP1_CREATE_APPLICATION.descriptor,
            "(J)Landroid/app/Application;"
        );
        assert_eq!(STEP2_CREATE_CONTENT_PROVIDERS.descriptor, "()V");
        assert_eq!(STEP3_APPLICATION_ON_CREATE.class, "android/app/Application");
        assert_eq!(STEP3_APPLICATION_ON_CREATE.descriptor, "()V");
        assert_eq!(
            STEP4_CREATE_MAIN_ACTIVITY.descriptor,
            "(Ljava/lang/String;JLjava/lang/String;)Landroid/app/Activity;"
        );
        assert_eq!(
            STEP5_ACTIVITY_ON_CREATE.descriptor,
            "(Landroid/os/Bundle;)V"
        );

        assert_eq!(STEP_ACTIVITY_ON_POST_CREATE.class, "android/app/Activity");
        assert_eq!(STEP_ACTIVITY_ON_POST_CREATE.method, "onPostCreate");
        assert_eq!(
            STEP_ACTIVITY_ON_POST_CREATE.descriptor,
            "(Landroid/os/Bundle;)V"
        );

        assert_eq!(STEP6_ACTIVITY_ON_START.class, "android/app/Activity");
        assert_eq!(STEP6_ACTIVITY_ON_START.method, "onStart");
        assert_eq!(STEP6_ACTIVITY_ON_START.descriptor, "()V");
        assert_eq!(STEP7_ACTIVITY_ON_RESUME.class, "android/app/Activity");
        assert_eq!(STEP7_ACTIVITY_ON_RESUME.method, "onResume");
        assert_eq!(STEP7_ACTIVITY_ON_RESUME.descriptor, "()V");
    }

    #[test]
    fn motion_action_codes_match_public_android_constants() {
        assert_eq!(MotionAction::Down.code(), 0, "ACTION_DOWN must be 0");
        assert_eq!(MotionAction::Up.code(), 1, "ACTION_UP must be 1");
    }

    #[test]
    fn motion_event_dispatch_descriptors_are_the_public_android_api() {
        assert_eq!(MOTION_EVENT_CLASS.to_str(), "android/view/MotionEvent");

        assert_eq!(jni_str!("obtain").to_str(), "obtain");
        assert_eq!(
            jni_sig!("(JJIFFI)Landroid/view/MotionEvent;")
                .sig()
                .to_str(),
            "(JJIFFI)Landroid/view/MotionEvent;"
        );

        assert_eq!(
            jni_str!("dispatchTouchEvent").to_str(),
            "dispatchTouchEvent"
        );
        assert_eq!(
            jni_sig!("(Landroid/view/MotionEvent;)Z").sig().to_str(),
            "(Landroid/view/MotionEvent;)Z"
        );

        assert_eq!(jni_str!("recycle").to_str(), "recycle");
        assert_eq!(jni_sig!("()V").sig().to_str(), "()V");

        assert_eq!(
            jni_str!("uptimeMillis").to_str(),
            UPTIME_MILLIS_NAME.to_str()
        );
        assert_eq!(jni_sig!("()J").sig().to_str(), "()J");
    }

    #[test]
    fn key_action_codes_match_public_android_constants() {
        assert_eq!(KeyAction::Down.code(), 0, "KeyEvent.ACTION_DOWN must be 0");
        assert_eq!(KeyAction::Up.code(), 1, "KeyEvent.ACTION_UP must be 1");
    }

    #[test]
    fn key_event_dispatch_descriptors_are_the_public_android_api() {
        assert_eq!(KEY_EVENT_CLASS.to_str(), "android/view/KeyEvent");

        assert_eq!(jni_sig!("(JJIIII)V").sig().to_str(), "(JJIIII)V");

        assert_eq!(jni_str!("dispatchKeyEvent").to_str(), "dispatchKeyEvent");
        assert_eq!(
            jni_sig!("(Landroid/view/KeyEvent;)Z").sig().to_str(),
            "(Landroid/view/KeyEvent;)Z"
        );

        assert_eq!(jni_str!("unicodeValue").to_str(), "unicodeValue");
        assert_eq!(INT_SIG.to_str(), "I");
    }

    const SINGLE_LINE: TextFieldRules = TextFieldRules {
        multiline: false,
        editable: true,
        caret_step: CaretStep::Grapheme,
    };

    const MULTILINE: TextFieldRules = TextFieldRules {
        multiline: true,
        editable: true,
        caret_step: CaretStep::Grapheme,
    };

    fn field(text: &str, cursor: usize) -> TextFieldState {
        TextFieldState {
            text: text.to_owned(),
            cursor,
            select_all: false,
            composing: None,
        }
    }

    fn selected(text: &str) -> TextFieldState {
        TextFieldState {
            select_all: true,
            ..field(text, 0)
        }
    }

    fn apply_all(
        state: TextFieldState,
        edits: &[TextEdit<'_>],
        rules: TextFieldRules,
    ) -> TextFieldState {
        edits
            .iter()
            .fold(state, |state, &edit| apply_text_edit(state, edit, rules))
    }

    fn text_and_cursor(state: TextFieldState) -> (String, usize) {
        (state.text, state.cursor)
    }

    #[test]
    fn typing_inserts_whole_key_text_at_the_cursor_and_drops_control_characters() {
        let typed = |text: &str, cursor, typed| {
            text_and_cursor(apply_text_edit(
                field(text, cursor),
                TextEdit::Type(typed),
                SINGLE_LINE,
            ))
        };
        assert_eq!(typed("", 0, "a"), ("a".to_owned(), 1));
        assert_eq!(typed("ro", 2, "b"), ("rob".to_owned(), 3));
        assert_eq!(typed("ro", 2, "\u{1b}"), ("ro".to_owned(), 2));
        assert_eq!(typed("é", 2, "x"), ("éx".to_owned(), 3));
        assert_eq!(typed("print()", 5, "x"), ("printx()".to_owned(), 6));
        assert_eq!(
            typed("ab", 1, "J\u{301}"),
            ("aJ\u{301}b".to_owned(), 1 + "J\u{301}".len())
        );
        assert_eq!(typed("ab", 1, "中文"), ("a中文b".to_owned(), 7));
    }

    #[test]
    fn backspace_and_delete_remove_one_character_or_one_word() {
        let edit = |text: &str, cursor, edit| {
            text_and_cursor(apply_text_edit(field(text, cursor), edit, SINGLE_LINE))
        };
        let back = TextEdit::Backspace(TextUnit::Character);
        let delete = TextEdit::ForwardDelete(TextUnit::Character);
        assert_eq!(edit("rob", 3, back), ("ro".to_owned(), 2));
        assert_eq!(edit("", 0, back), (String::new(), 0));
        assert_eq!(edit("a💫b", 5, back), ("ab".to_owned(), 1));
        assert_eq!(edit("abc", 0, delete), ("bc".to_owned(), 0));
        assert_eq!(edit("abc", 3, delete), ("abc".to_owned(), 3));
        assert_eq!(edit("a💫b", 1, delete), ("ab".to_owned(), 1));
        assert_eq!(
            edit("hello world", 11, TextEdit::Backspace(TextUnit::Word)),
            ("hello ".to_owned(), 6)
        );
        assert_eq!(
            edit("hello world", 0, TextEdit::ForwardDelete(TextUnit::Word)),
            (" world".to_owned(), 0)
        );
        assert_eq!(
            edit("foo.bar baz", 7, TextEdit::Backspace(TextUnit::Word)),
            ("foo. baz".to_owned(), 4)
        );
    }

    #[test]
    fn keys_step_over_whole_emoji_and_backspace_removes_one_combining_mark() {
        let edit = |text: &str, cursor, edit| {
            text_and_cursor(apply_text_edit(field(text, cursor), edit, MULTILINE))
        };
        let back = TextEdit::Backspace(TextUnit::Character);
        let delete = TextEdit::ForwardDelete(TextUnit::Character);
        let left = TextEdit::Move(TextMotion::Left(TextUnit::Character));
        let right = TextEdit::Move(TextMotion::Right(TextUnit::Character));
        for emoji in [
            "\u{2764}\u{FE0F}",
            "\u{1F1FA}\u{1F1F8}",
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
            "\u{1F44D}\u{1F3FD}",
            "1\u{FE0F}\u{20E3}",
        ] {
            let text = format!("a{emoji}b");
            let after = 1 + emoji.len();
            assert_eq!(edit(&text, after, back), ("ab".to_owned(), 1), "{emoji:?}");
            assert_eq!(edit(&text, 1, delete), ("ab".to_owned(), 1), "{emoji:?}");
            assert_eq!(edit(&text, 1, right).1, after, "{emoji:?}");
            assert_eq!(edit(&text, after, left).1, 1, "{emoji:?}");
        }
        assert_eq!(edit("ae\u{301}", 4, back), ("ae".to_owned(), 2));
        assert_eq!(edit("e\u{301}x", 0, delete), ("x".to_owned(), 0));
        assert_eq!(edit("e\u{301}x", 0, right).1, 3);
        assert_eq!(
            edit("x cafe\u{301}", 8, TextEdit::Backspace(TextUnit::Word)),
            ("x ".to_owned(), 2)
        );
        assert_eq!(
            edit("e\u{301}x\nabc", 6, TextEdit::Move(TextMotion::Up)).1,
            3
        );
        assert_eq!(
            edit("e\u{301}x\nabc", 3, TextEdit::Move(TextMotion::Down)).1,
            6
        );
    }

    #[test]
    fn arrow_keys_move_the_caret_so_typing_fixes_a_typo_mid_text() {
        let left = TextEdit::Move(TextMotion::Left(TextUnit::Character));
        let edits: Vec<_> = std::iter::repeat_n(left, 7)
            .chain([TextEdit::Type("l")])
            .collect();
        let fixed = apply_all(field("helo world", 10), &edits, SINGLE_LINE);
        assert_eq!(text_and_cursor(fixed), ("hello world".to_owned(), 4));
    }

    #[test]
    fn the_caret_moves_by_whole_characters_words_and_lines() {
        let moved = |text: &str, cursor, motion| {
            apply_text_edit(field(text, cursor), TextEdit::Move(motion), MULTILINE).cursor
        };
        assert_eq!(moved("a💫b", 5, TextMotion::Left(TextUnit::Character)), 1);
        assert_eq!(moved("a💫b", 1, TextMotion::Right(TextUnit::Character)), 5);
        assert_eq!(moved("a💫b", 0, TextMotion::Left(TextUnit::Character)), 0);
        assert_eq!(moved("a💫b", 6, TextMotion::Right(TextUnit::Character)), 6);
        assert_eq!(moved("a💫b", 5, TextMotion::TextStart), 0);
        assert_eq!(moved("a💫b", 1, TextMotion::TextEnd), 6);
        assert_eq!(
            moved("hello world", 11, TextMotion::Left(TextUnit::Word)),
            6
        );
        assert_eq!(
            moved("hello world", 0, TextMotion::Right(TextUnit::Word)),
            5
        );
        assert_eq!(moved("ab\ncd", 4, TextMotion::LineStart), 3);
        assert_eq!(moved("ab\ncd", 1, TextMotion::LineEnd), 2);
        assert_eq!(moved("ab\ncd", 4, TextMotion::Up), 1);
        assert_eq!(moved("ab\ncd", 1, TextMotion::Down), 4);
        assert_eq!(moved("abc\nd", 3, TextMotion::Down), 5);
        assert_eq!(moved("ab\ncd", 1, TextMotion::Up), 0);
        assert_eq!(moved("ab\ncd", 4, TextMotion::Down), 5);
    }

    #[test]
    fn line_keys_in_a_wrapped_box_move_by_the_lines_the_overlay_draws() {
        if crate::host_fonts::system_font().is_none() {
            eprintln!("SKIP: no host font for the text layout");
            return;
        }
        let session = TextboxSession {
            geometry: (0, 0, 120, 400),
            multiline: true,
            text_wrapped: true,
            ..chat_bar_session(47)
        };
        let text = "a".repeat(60);
        let moved = |motion| line_moved_cursor(session, &field(&text, 30), motion).expect("layout");
        let up = moved(crate::text_layout::LineMotion::Up);
        let down = moved(crate::text_layout::LineMotion::Down);
        let home = moved(crate::text_layout::LineMotion::Start);
        let end = moved(crate::text_layout::LineMotion::End);
        assert!(0 < up && up < home, "up {up}, home {home}");
        assert!(
            home <= 30 && 30 < end && end < down && down < 60,
            "{home} {end} {down}"
        );
        assert_eq!(
            line_moved_cursor(
                session,
                &selected(&text),
                crate::text_layout::LineMotion::Up
            ),
            None
        );
        assert_eq!(
            line_motion(TextEdit::Move(TextMotion::Up)),
            Some(crate::text_layout::LineMotion::Up)
        );
        assert_eq!(line_motion(TextEdit::Move(TextMotion::TextStart)), None);
    }

    #[test]
    fn line_keys_in_a_wrapped_box_move_the_focused_caret_by_the_drawn_lines() {
        if crate::host_fonts::system_font().is_none() {
            eprintln!("SKIP: no host font for the text layout");
            return;
        }
        let text = "a".repeat(60);
        let (up, end) = with_focused_text_field(|_env, handle| {
            let session = TextboxSession {
                geometry: (0, 0, 120, 400),
                multiline: true,
                text_wrapped: true,
                ..chat_bar_session(handle)
            };
            view_registry::with_view(handle, |view| view.text = Some(text.clone()))
                .expect("set text");
            let moved = |motion| {
                ACTIVE_TEXT_CURSOR_UTF16.store(30, std::sync::atomic::Ordering::Release);
                let (_, cursor, _) = edit_text_box(session, TextEdit::Move(motion)).expect("edit");
                cursor
            };
            (moved(TextMotion::Up), moved(TextMotion::LineEnd))
        });
        assert!(
            0 < up && up < 30 && 30 < end && end < 60,
            "up {up}, end {end}"
        );
    }

    #[test]
    fn arrows_in_a_masked_box_step_one_bullet_per_char() {
        const PASSWORD: i32 = 5;
        const PLAIN: i32 = 0;
        let left = TextEdit::Move(TextMotion::Left(TextUnit::Character));
        let right = TextEdit::Move(TextMotion::Right(TextUnit::Character));
        let back = TextEdit::Backspace(TextUnit::Character);
        let edited = with_focused_text_field(|_env, handle| {
            let edit = |input_type, from, edit| {
                view_registry::with_view(handle, |view| {
                    view.text = Some("a\u{2764}\u{FE0F}".to_owned());
                })
                .expect("set text");
                ACTIVE_TEXT_CURSOR_UTF16.store(from, std::sync::atomic::Ordering::Release);
                let session = TextboxSession {
                    input_type,
                    ..chat_bar_session(handle)
                };
                let (text, cursor, _) = edit_text_box(session, edit).expect("edit");
                (text, cursor)
            };
            [
                edit(PASSWORD, 1, right),
                edit(PASSWORD, 3, left),
                edit(PASSWORD, 3, back),
                edit(PLAIN, 1, right),
                edit(PLAIN, 3, left),
            ]
        });
        let heart = "a\u{2764}\u{FE0F}".to_owned();
        assert_eq!(
            edited,
            [
                (heart.clone(), 2),
                (heart.clone(), 2),
                ("a".to_owned(), 1),
                (heart.clone(), 3),
                (heart, 1),
            ]
        );
    }

    #[test]
    fn navigation_collapses_select_all_instead_of_replacing_the_text() {
        let right = TextEdit::Move(TextMotion::Right(TextUnit::Character));
        let appended = apply_all(
            selected("hello world"),
            &[right, TextEdit::Type("x")],
            SINGLE_LINE,
        );
        assert_eq!(text_and_cursor(appended), ("hello worldx".to_owned(), 12));

        let home = apply_text_edit(
            selected("hello"),
            TextEdit::Move(TextMotion::Left(TextUnit::Character)),
            SINGLE_LINE,
        );
        assert_eq!((home.cursor, home.select_all), (0, false));

        let replaced = apply_text_edit(selected("print('old')"), TextEdit::Type("x"), SINGLE_LINE);
        assert_eq!(text_and_cursor(replaced), ("x".to_owned(), 1));
        let cleared = apply_text_edit(
            selected("éx"),
            TextEdit::ForwardDelete(TextUnit::Character),
            SINGLE_LINE,
        );
        assert_eq!(text_and_cursor(cleared), (String::new(), 0));
    }

    #[test]
    fn ime_preedit_is_shown_inline_and_replaced_by_the_commit() {
        let composing = apply_text_edit(
            field("ab", 1),
            TextEdit::Compose {
                text: "に",
                cursor: None,
            },
            SINGLE_LINE,
        );
        assert_eq!(composing.text, "aにb");
        assert_eq!(composing.composing, Some(1..4));
        assert_eq!(composing.cursor, 4);

        let converted = apply_text_edit(
            composing,
            TextEdit::Compose {
                text: "日本",
                cursor: Some(3),
            },
            SINGLE_LINE,
        );
        assert_eq!(converted.text, "a日本b");
        assert_eq!(converted.composing, Some(1..7));
        assert_eq!(converted.cursor, 4);

        let committed = apply_all(
            converted,
            &[
                TextEdit::Compose {
                    text: "",
                    cursor: None,
                },
                TextEdit::Type("日本"),
            ],
            SINGLE_LINE,
        );
        assert_eq!(text_and_cursor(committed.clone()), ("a日本b".to_owned(), 7));
        assert_eq!(committed.composing, None);

        let kept = apply_all(
            field("ab", 1),
            &[
                TextEdit::Compose {
                    text: "x",
                    cursor: None,
                },
                TextEdit::FinishComposing,
            ],
            SINGLE_LINE,
        );
        assert_eq!(text_and_cursor(kept.clone()), ("axb".to_owned(), 2));
        assert_eq!(kept.composing, None);
    }

    #[test]
    fn an_empty_preedit_without_a_composition_keeps_the_selection() {
        let cleared = TextEdit::Compose {
            text: "",
            cursor: None,
        };
        assert_eq!(
            apply_text_edit(selected("abc"), cleared, SINGLE_LINE),
            selected("abc")
        );
        let committed = apply_all(
            selected("abc"),
            &[cleared, TextEdit::Type("x")],
            SINGLE_LINE,
        );
        assert_eq!(text_and_cursor(committed), ("x".to_owned(), 1));
    }

    #[test]
    fn paste_keeps_line_breaks_only_in_multiline_boxes_and_respects_read_only_boxes() {
        let pasted = |rules| {
            apply_text_edit(field("", 0), TextEdit::Paste("hi\r\nyou\tthere\rok"), rules).text
        };
        assert_eq!(pasted(SINGLE_LINE), "hiyouthereok");
        assert_eq!(pasted(MULTILINE), "hi\nyou\tthere\nok");

        let read_only = TextFieldRules {
            editable: false,
            ..SINGLE_LINE
        };
        assert_eq!(
            apply_text_edit(field("code", 4), TextEdit::Paste("x"), read_only).text,
            "code"
        );
        assert_eq!(
            apply_text_edit(field("code", 4), TextEdit::Type("x"), read_only).text,
            "codex"
        );
    }

    #[test]
    fn engine_presents_wake_the_main_loop_only_while_armed() {
        let armed = std::sync::atomic::AtomicBool::new(false);
        let wakes = std::cell::Cell::new(0);
        let present = || wake_when_armed(&armed, || wakes.set(wakes.get() + 1));

        present();
        assert_eq!(wakes.get(), 0);
        armed.store(true, std::sync::atomic::Ordering::Relaxed);
        present();
        present();
        assert_eq!(wakes.get(), 2);
        armed.store(false, std::sync::atomic::Ordering::Relaxed);
        present();
        assert_eq!(wakes.get(), 2);
    }

    #[test]
    fn edits_stop_at_the_64000_utf16_unit_length_filter_of_the_apk_keyboard() {
        let utf16 = |state: &TextFieldState| state.text.encode_utf16().count();
        let huge = "ab".repeat(ROBLOX_TEXT_BOX_MAX_UTF16);

        let pasted = apply_text_edit(field("xy", 1), TextEdit::Paste(&huge), SINGLE_LINE);
        assert_eq!(utf16(&pasted), ROBLOX_TEXT_BOX_MAX_UTF16);
        assert!(pasted.text.starts_with("xab") && pasted.text.ends_with("y"));
        assert_eq!(pasted.cursor, ROBLOX_TEXT_BOX_MAX_UTF16 - 1);

        let replaced = apply_text_edit(selected(&huge[..10]), TextEdit::Paste(&huge), SINGLE_LINE);
        assert_eq!(utf16(&replaced), ROBLOX_TEXT_BOX_MAX_UTF16);

        let full = field(&huge[..ROBLOX_TEXT_BOX_MAX_UTF16], 3);
        assert_eq!(
            apply_text_edit(full.clone(), TextEdit::Type("z"), SINGLE_LINE),
            full
        );

        let one_left = field(&huge[..ROBLOX_TEXT_BOX_MAX_UTF16 - 1], 0);
        assert_eq!(
            apply_text_edit(one_left.clone(), TextEdit::Type("😀"), SINGLE_LINE),
            one_left
        );
        assert_eq!(
            utf16(&apply_text_edit(
                one_left.clone(),
                TextEdit::Type("é😀"),
                SINGLE_LINE
            )),
            ROBLOX_TEXT_BOX_MAX_UTF16
        );

        let composing = apply_all(
            one_left,
            &[
                TextEdit::Compose {
                    text: "に",
                    cursor: None,
                },
                TextEdit::Compose {
                    text: "日本",
                    cursor: Some(3),
                },
            ],
            SINGLE_LINE,
        );
        assert!(composing.text.starts_with("日a"));
        assert_eq!(utf16(&composing), ROBLOX_TEXT_BOX_MAX_UTF16);
        assert_eq!(composing.composing, Some(0..3));
        assert_eq!(composing.cursor, 3);
    }

    #[test]
    fn copy_takes_only_a_visible_non_empty_selection() {
        assert_eq!(selected_text(&selected("hunter2"), false), Some("hunter2"));
        assert_eq!(selected_text(&selected("hunter2"), true), None);
        assert_eq!(selected_text(&field("hunter2", 7), false), None);
        assert_eq!(selected_text(&selected(""), false), None);
        assert!(text_input_type_masks_text(5));
        assert!(!text_input_type_masks_text(7));
    }

    fn chat_bar_session(widget: i64) -> TextboxSession {
        TextboxSession {
            widget,
            geometry: (181, 155, 438, 46),
            input_type: 0,
            font: 39,
            font_size: 20.0,
            multiline: false,
            text_wrapped: false,
            text_color: -1,
            x_alignment: 0,
            y_alignment: 1,
            editable: true,
        }
    }

    fn with_focused_text_field<T>(test: impl FnOnce(&mut Env, i64) -> T) -> T {
        let _textbox_test_guard = TEXTBOX_TEST_LOCK.lock().expect("textbox test lock");
        fake_jvm::with_env(|env| {
            let handle = view_registry::allocate("android.widget.EditText").expect("allocate");
            ACTIVE_TEXT_FIELD.store(handle, std::sync::atomic::Ordering::Release);
            record_textbox_session(Some(chat_bar_session(handle)));
            let result = test(env, handle);
            clear_active_text_field();
            view_registry::free(handle).expect("free");
            result
        })
    }

    #[test]
    fn roblox_hiding_its_keyboard_view_ends_the_focus_session() {
        with_focused_text_field(|_env, handle| {
            view_native_set_visibility(
                fake_jvm::native_env(),
                JObject::null(),
                handle,
                8,
                1.0,
                true,
            );

            assert_eq!(active_text_field(), 0);
            assert_eq!(textbox_geometry(), None);
            assert!(active_text_overlay().is_none());
        });
    }

    #[test]
    fn showing_the_keyboard_view_again_requeries_the_text_box_without_dropping_it() {
        with_focused_text_field(|_env, handle| {
            assert!(!textbox_geometry_pending());
            view_native_set_visibility(
                fake_jvm::native_env(),
                JObject::null(),
                handle,
                0,
                1.0,
                true,
            );

            assert!(textbox_geometry_pending());
            assert_eq!(active_text_field(), handle);
            assert_eq!(textbox_geometry(), Some((181, 155, 438, 46)));
        });
    }

    #[test]
    fn a_relayout_of_the_keyboard_view_requeries_the_text_box() {
        with_focused_text_field(|_env, handle| {
            view_native_set_layout_params(
                fake_jvm::native_env(),
                JObject::null(),
                handle,
                438,
                46,
                -1,
                0.0,
                381,
                155,
                0,
                0,
            );

            assert!(textbox_geometry_pending());
            assert_eq!(textbox_geometry(), Some((181, 155, 438, 46)));
        });
    }

    #[test]
    fn show_keyboard_for_another_box_with_the_same_text_requeries_the_text_box() {
        with_focused_text_field(|env, handle| {
            let empty = env.new_string("").expect("text");
            widget_native_set_text(fake_jvm::native_env(), JObject::null(), handle, empty);

            assert!(textbox_geometry_pending());
            assert_eq!(active_text_field(), handle);
        });
    }

    #[test]
    fn a_zero_size_text_box_neither_polls_nor_takes_keys() {
        let _textbox_test_guard = TEXTBOX_TEST_LOCK.lock().expect("textbox test lock");
        ACTIVE_TEXT_FIELD.store(46, std::sync::atomic::Ordering::Release);
        record_textbox_session(Some(TextboxSession {
            geometry: (10, 20, 0, 0),
            ..chat_bar_session(46)
        }));

        assert!(!textbox_geometry_pending());
        assert_eq!(live_text_box_session(), None);
        assert!(active_text_overlay().is_none());
        assert!(clear_active_text_field());
    }

    #[test]
    fn enter_runs_the_retained_editor_action_listener_outside_the_view_registry() {
        let (probe_result, probe) = std::sync::mpsc::channel();
        let released = with_focused_text_field(|env, handle| {
            let view = fake_jvm::new_object(env);
            let peer = env.new_weak_ref(&view).expect("weak ref");
            view_registry::set_jobject(handle, view_registry::ViewObject::Peer(peer))
                .expect("record peer");
            let listener = fake_jvm::new_object(env);
            let retained = env.new_global_ref(&listener).expect("global ref");
            view_registry::set_editor_action_listener(handle, Some(retained))
                .expect("retain listener");
            fake_jvm::on_call(&listener, "onEditorAction", move || {
                let (reached, registry_reached) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    reached
                        .send(view_registry::with_view(handle, |_| ()).is_ok())
                        .ok();
                });
                let reached_in_time =
                    registry_reached.recv_timeout(std::time::Duration::from_secs(5));
                probe_result.send(reached_in_time == Ok(true)).ok();
                view_native_set_visibility(
                    fake_jvm::native_env(),
                    JObject::null(),
                    handle,
                    8,
                    1.0,
                    true,
                );
            });

            let outcome = run_editor_action(env, handle);
            (outcome.is_ok(), active_text_field())
        });
        assert_eq!(released, (true, 0), "the listener ran and released the box");
        assert_eq!(
            probe.try_recv(),
            Ok(true),
            "onEditorAction must not run while the view registry is locked"
        );
    }

    #[test]
    fn roblox_clipboard_writes_are_queued_for_the_host_clipboard() {
        fake_jvm::with_env(|env| {
            let copied = env.new_string("roblox.com/share").expect("text");
            clipboard_manager_native_set_clipboard(fake_jvm::native_env(), JClass::null(), copied);
        });
        assert_eq!(
            take_pending_host_clipboard_text().as_deref(),
            Some("roblox.com/share")
        );
        assert_eq!(take_pending_host_clipboard_text(), None);
    }

    #[test]
    fn escape_releases_the_text_box_through_the_back_key_pre_ime_path() {
        let released = with_focused_text_field(|env, handle| {
            let view = fake_jvm::new_object(env);
            let peer = env.new_weak_ref(&view).expect("weak ref");
            view_registry::set_jobject(handle, view_registry::ViewObject::Peer(peer))
                .expect("record peer");
            fake_jvm::on_call(&view, "onKeyPreIme", move || {
                view_native_set_visibility(
                    fake_jvm::native_env(),
                    JObject::null(),
                    handle,
                    8,
                    1.0,
                    true,
                );
            });

            let outcome = release_back_on_text_field(env, handle);
            (outcome.ok(), active_text_field())
        });
        assert_eq!(released, (Some(true), 0));
    }

    #[test]
    fn host_text_input_uses_the_apk_non_composing_synchronization_route() {
        let src = include_str!("framework.rs");
        let input_start = src
            .find("pub(crate) fn edit_active_text_field")
            .expect("host text-input entry point");
        let input_end = src[input_start..]
            .find("\nfn sync_engine_textbox")
            .map(|offset| input_start + offset)
            .expect("engine textbox synchronization helper");
        let input = &src[input_start..input_end];
        assert!(input.contains("sync_engine_textbox(vm, &text, cursor)"));
        assert!(!input.contains("fire_text_watchers"));

        let sync_start = input_end;
        let sync_end = src[sync_start..]
            .find("\npub fn reflect_engine_input_methods")
            .map(|offset| sync_start + offset)
            .expect("input reflection helper");
        let sync = &src[sync_start..sync_end];
        assert!(sync.contains("syncTextboxTextAndCursorPosition2"));
        assert!(!sync.contains("nativePassText"));
        assert!(!sync.contains("JObject::from_raw"));
    }

    #[test]
    fn focused_textbox_geometry_is_read_without_reflection_marshalling() {
        let source = include_str!("framework.rs");
        let start = source
            .find("pub fn query_textbox_geometry")
            .expect("textbox geometry query");
        let end = source[start..]
            .find("\nextern \"system\" fn widget_native_set_text")
            .map(|offset| start + offset)
            .expect("textbox geometry query boundary");
        let query = &source[start..end];
        assert!(query.contains("if !refresh && has_live_textbox_session(widget)"));
        assert!(query.contains("env.get_field"));
        assert!(!query.contains("getDeclaredField"));
        assert!(!query.contains("setAccessible"));
    }

    #[test]
    fn java_cursor_position_counts_utf16_code_units() {
        assert_eq!(java_cursor_position("Karma"), 5);
        assert_eq!(java_cursor_position("é"), 1);
        assert_eq!(java_cursor_position("💫"), 2);
    }

    #[test]
    fn bootstrap_class_constants_are_slashed_internal_names() {
        assert_eq!(CONTEXT_CLASS.to_str(), "android/content/Context");
        assert_eq!(APPLICATION_CLASS.to_str(), "android/app/Application");
    }

    #[test]
    fn call_site_literals_match_recipe_constants() {
        assert_eq!(
            jni_str!("createApplication").to_str(),
            STEP1_CREATE_APPLICATION.method
        );
        assert_eq!(
            jni_sig!("(J)Landroid/app/Application;").sig().to_str(),
            STEP1_CREATE_APPLICATION.descriptor
        );
        assert_eq!(
            jni_str!("createContentProviders").to_str(),
            STEP2_CREATE_CONTENT_PROVIDERS.method
        );
        assert_eq!(
            jni_sig!("()V").sig().to_str(),
            STEP2_CREATE_CONTENT_PROVIDERS.descriptor
        );
        assert_eq!(
            jni_str!("onCreate").to_str(),
            STEP3_APPLICATION_ON_CREATE.method
        );
        assert_eq!(
            jni_sig!("()V").sig().to_str(),
            STEP3_APPLICATION_ON_CREATE.descriptor
        );

        assert_eq!(
            jni_str!("createMainActivity").to_str(),
            STEP4_CREATE_MAIN_ACTIVITY.method
        );
        assert_eq!(
            jni_sig!("(Ljava/lang/String;JLjava/lang/String;)Landroid/app/Activity;")
                .sig()
                .to_str(),
            STEP4_CREATE_MAIN_ACTIVITY.descriptor
        );
        assert_eq!(
            jni_str!("onCreate").to_str(),
            STEP5_ACTIVITY_ON_CREATE.method
        );
        assert_eq!(
            jni_sig!("(Landroid/os/Bundle;)V").sig().to_str(),
            STEP5_ACTIVITY_ON_CREATE.descriptor
        );

        assert_eq!(
            jni_str!("onPostCreate").to_str(),
            STEP_ACTIVITY_ON_POST_CREATE.method
        );
        assert_eq!(
            jni_sig!("(Landroid/os/Bundle;)V").sig().to_str(),
            STEP_ACTIVITY_ON_POST_CREATE.descriptor
        );

        assert_eq!(jni_str!("onStart").to_str(), STEP6_ACTIVITY_ON_START.method);
        assert_eq!(
            jni_sig!("()V").sig().to_str(),
            STEP6_ACTIVITY_ON_START.descriptor
        );
        assert_eq!(
            jni_str!("onResume").to_str(),
            STEP7_ACTIVITY_ON_RESUME.method
        );
        assert_eq!(
            jni_sig!("()V").sig().to_str(),
            STEP7_ACTIVITY_ON_RESUME.descriptor
        );

        assert_eq!(ACTIVITY_CLASS.to_str(), "android/app/Activity");
        assert_eq!(STEP4_CREATE_MAIN_ACTIVITY.class, "android/app/Activity");
        assert_eq!(STEP5_ACTIVITY_ON_CREATE.class, "android/app/Activity");
        assert_eq!(STEP_ACTIVITY_ON_POST_CREATE.class, "android/app/Activity");
        assert_eq!(STEP6_ACTIVITY_ON_START.class, "android/app/Activity");
        assert_eq!(STEP7_ACTIVITY_ON_RESUME.class, "android/app/Activity");
    }

    #[test]
    fn lifecycle_drivers_call_on_post_create_between_on_create_and_on_start() {
        let src = include_str!("framework.rs");

        fn assert_order(src: &str, marker: &str) {
            let start = src
                .find(marker)
                .unwrap_or_else(|| panic!("missing {marker}"));

            let rest = &src[start + marker.len()..];
            let end = rest
                .find("\nfn ")
                .map_or(src.len(), |o| start + marker.len() + o);
            let body = &src[start..end];
            let create = body
                .find("call_activity_on_create(")
                .unwrap_or_else(|| panic!("{marker}: no call_activity_on_create"));
            let post = body
                .find("call_activity_on_post_create(")
                .unwrap_or_else(|| panic!("{marker}: no call_activity_on_post_create"));
            let start_call = body
                .find("call_activity_on_start(")
                .unwrap_or_else(|| panic!("{marker}: no call_activity_on_start"));
            let resume = body
                .find("call_activity_on_resume(")
                .unwrap_or_else(|| panic!("{marker}: no call_activity_on_resume"));
            assert!(
                create < post && post < start_call && start_call < resume,
                "{marker}: drive order must be onCreate < onPostCreate < onStart < onResume \
                 (got create={create} post={post} start={start_call} resume={resume})"
            );
        }

        assert_order(src, "fn activity_native_start_activity<'local>");
        assert_order(
            src,
            "call_activity_on_create(env, &activity, \"step 5 Activity.onCreate\")",
        );
    }

    #[test]
    fn a_deleted_theme_handle_is_released() {
        fake_jvm::with_env(register_asset_manager_natives).expect("register AssetManager");
        let native =
            fake_jvm::registered_native("android/content/res/AssetManager", "deleteTheme", "(J)V")
                .expect("AssetManager.deleteTheme is bound");
        let delete_theme: extern "system" fn(EnvUnowned<'static>, JObject<'static>, jlong) =
            unsafe { std::mem::transmute(native) };

        let theme = theme_registry::allocate().expect("allocate theme");
        delete_theme(fake_jvm::native_env(), JObject::null(), theme);
        assert_eq!(
            theme_registry::with_theme(theme, |_| ()),
            Err(theme_registry::ThemeRegistryError::StaleHandle)
        );

        delete_theme(fake_jvm::native_env(), JObject::null(), 0);
    }

    #[test]
    fn resolve_theme_attr_returns_concrete_values_and_none_for_missing() {
        use crate::framework::theme_registry::ThemeAttr;
        let mut attrs = std::collections::HashMap::new();

        let win_action_bar = u32_to_i32(0x7f01_0058);
        attrs.insert(
            win_action_bar,
            ThemeAttr {
                type_: 0x12,
                data: 0xffff_ffff,
                source_package: 0x7f,
            },
        );
        let e = resolve_theme_attr(&attrs, win_action_bar, &ResConfig::default())
            .expect("present attr resolves");
        assert_eq!(e.value_type, 0x12, "TYPE_INT_BOOLEAN preserved");
        assert_eq!(
            e.data,
            u32_to_i32(0xffff_ffff),
            "boolean true data preserved"
        );
        assert_eq!(e.resource_id, 0, "a concrete value has no resource id");

        assert!(
            resolve_theme_attr(&attrs, u32_to_i32(0x7f01_9999), &ResConfig::default()).is_none(),
            "an attribute absent from the theme must be None, not a fabricated value"
        );
    }

    #[test]
    fn resolve_theme_attr_follows_theme_attribute_indirection_and_breaks_cycles() {
        use crate::framework::theme_registry::ThemeAttr;
        let mut attrs = std::collections::HashMap::new();
        let alias = u32_to_i32(0x7f01_0001);
        let target = u32_to_i32(0x7f01_0002);

        attrs.insert(
            alias,
            ThemeAttr {
                type_: TYPE_ATTRIBUTE,
                data: u32::from_ne_bytes(target.to_ne_bytes()),
                source_package: 0x7f,
            },
        );
        attrs.insert(
            target,
            ThemeAttr {
                type_: 0x10,
                data: 7,
                source_package: 0x7f,
            },
        );
        let e =
            resolve_theme_attr(&attrs, alias, &ResConfig::default()).expect("indirection resolves");
        assert_eq!(e.value_type, 0x10, "resolved to the target's concrete type");
        assert_eq!(e.data, 7, "resolved to the target's concrete data");

        let mut cyc = std::collections::HashMap::new();
        let a = u32_to_i32(0x7f01_00aa);
        cyc.insert(
            a,
            ThemeAttr {
                type_: TYPE_ATTRIBUTE,
                data: u32::from_ne_bytes(a.to_ne_bytes()),
                source_package: 0x7f,
            },
        );

        let e = resolve_theme_attr(&cyc, a, &ResConfig::default())
            .expect("cycle terminates with a value");
        assert_eq!(e.value_type, i32::from(TYPE_ATTRIBUTE));
    }

    #[test]
    fn styled_type_string_cookie_routes_to_the_owning_pool() {
        use crate::framework::theme_registry::ThemeAttr;

        let mut attrs = std::collections::HashMap::new();
        let app_attr = u32_to_i32(0x7f01_0010);
        attrs.insert(
            app_attr,
            ThemeAttr {
                type_: TYPE_STRING,
                data: 0x456,
                source_package: 0x7f,
            },
        );
        let fw_attr = u32_to_i32(0x0101_0010);
        attrs.insert(
            fw_attr,
            ThemeAttr {
                type_: TYPE_STRING,
                data: 7,
                source_package: 0x01,
            },
        );
        let e = resolve_theme_attr(&attrs, app_attr, &ResConfig::default())
            .expect("app-table string resolves");
        assert_eq!(
            e.asset_cookie, ARSC_APP_COOKIE,
            "an app-table theme string must carry the app ARSC cookie (was -1 → the null-string bug)"
        );
        let e = resolve_theme_attr(&attrs, fw_attr, &ResConfig::default())
            .expect("framework-table string resolves");
        assert_eq!(
            e.asset_cookie, ARSC_FRAMEWORK_COOKIE,
            "a framework-table theme string must carry the framework ARSC cookie"
        );

        let inline = resolve_inline_attr_value(TYPE_STRING, 42, &ResConfig::default());
        assert_eq!(
            inline.asset_cookie, XML_BLOCK_COOKIE,
            "an inline XmlBlock string keeps the XmlBlock cookie"
        );

        let non_string = resolve_inline_attr_value(0x10, 5, &ResConfig::default());
        assert_eq!(non_string.asset_cookie, 0);
    }

    #[test]
    fn png_dimensions_parses_ihdr_and_rejects_non_png() {
        let mut png = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&48u32.to_be_bytes());
        png.extend_from_slice(&24u32.to_be_bytes());
        assert_eq!(png_dimensions(&png), Some((48, 24)));

        assert_eq!(png_dimensions(b"not a png at all"), None, "wrong signature");
        assert_eq!(png_dimensions(&png[..12]), None, "truncated before IHDR");
        let mut wrong_chunk = png.clone();
        wrong_chunk[12..16].copy_from_slice(b"IDAT");
        assert_eq!(png_dimensions(&wrong_chunk), None, "first chunk not IHDR");
    }

    #[test]
    fn bitmap_native_names_sigs_and_classes_match_bitmap_java() {
        assert_eq!(
            BITMAP_FACTORY_CLASS.to_str(),
            "android/graphics/BitmapFactory"
        );
        assert_eq!(BITMAP_CLASS.to_str(), "android/graphics/Bitmap");
        assert_eq!(
            BITMAP_FACTORY_DECODE_STREAM_NAME.to_str(),
            "nativeDecodeStream"
        );
        assert_eq!(
            BITMAP_FACTORY_DECODE_STREAM_SIG.to_str(),
            "(Ljava/io/InputStream;[BLandroid/graphics/Rect;Landroid/graphics/BitmapFactory$Options;)J"
        );
        assert_eq!(BITMAP_GET_WIDTH_NAME.to_str(), "native_get_width");
        assert_eq!(BITMAP_GET_WIDTH_SIG.to_str(), "(J)I");
        assert_eq!(BITMAP_GET_HEIGHT_NAME.to_str(), "native_get_height");
        assert_eq!(BITMAP_GET_HEIGHT_SIG.to_str(), "(J)I");

        assert_eq!(BITMAP_RECYCLE_NAME.to_str(), "native_recycle");
        assert_eq!(BITMAP_RECYCLE_SIG.to_str(), "(JJ)V");
        assert_eq!(BITMAP_CREATE_TEXTURE_NAME.to_str(), "native_create_texture");
        assert_eq!(BITMAP_CREATE_TEXTURE_SIG.to_str(), "(JIIII)J");
        assert_eq!(
            BITMAP_CREATE_SNAPSHOT_NAME.to_str(),
            "native_create_snapshot"
        );
        assert_eq!(BITMAP_CREATE_SNAPSHOT_SIG.to_str(), "(J)J");
        assert_eq!(BITMAP_REF_TEXTURE_NAME.to_str(), "native_ref_texture");
        assert_eq!(BITMAP_REF_TEXTURE_SIG.to_str(), "(J)J");
    }

    #[test]
    fn record_bitmap_from_file_records_dimensions_and_is_total_on_bad_paths() {
        let mut png = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&64u32.to_be_bytes());
        png.extend_from_slice(&32u32.to_be_bytes());
        let dir =
            std::env::temp_dir().join(format!("eclipse-record-bitmap-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("probe.png");
        std::fs::write(&path, &png).expect("write probe png");

        let handle = record_bitmap_from_file(&path.to_string_lossy(), "test");
        assert_ne!(handle, 0, "a readable PNG must yield a live handle");
        let (w, h) = bitmap_registry::with_bitmap(handle, |s| (s.width, s.height))
            .expect("recorded state readable");
        assert_eq!((w, h), (64, 32), "IHDR dimensions recorded");
        bitmap_registry::free(handle).expect("free recorded bitmap");
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();

        let missing = dir.join("does-not-exist.png");
        assert_eq!(
            record_bitmap_from_file(&missing.to_string_lossy(), "test"),
            0,
            "an unreadable path is the tolerated 0 (no paintable), never a panic"
        );
    }

    #[test]
    fn file_path_paintable_is_freed_when_its_drawable_releases_it() {
        let dir = std::env::temp_dir().join(format!(
            "eclipse-paintable-release-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("probe.9.png");
        std::fs::write(&path, b"not-a-png").expect("write probe file");
        let paintable = record_bitmap_from_file(&path.to_string_lossy(), "test");
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
        assert_ne!(paintable, 0, "a readable file yields a live paintable");

        retain_paintable(paintable);
        release_paintable(paintable);
        assert_eq!(
            bitmap_registry::with_bitmap(paintable, |_| ()),
            Err(bitmap_registry::BitmapRegistryError::StaleHandle),
            "the finalized Drawable's paintable no longer pins the file bytes"
        );

        let texture = bitmap_registry::store(bitmap_registry::BitmapState::default())
            .expect("store bitmap texture");
        retain_paintable(texture);
        release_paintable(texture);
        assert_eq!(
            bitmap_registry::with_bitmap(texture, |_| ()),
            Ok(()),
            "a BitmapDrawable release leaves the Bitmap's texture alive"
        );
        bitmap_registry::free(texture).expect("recycle texture");

        for sentinel in [
            0,
            DRAWABLE_HANDLE_SENTINEL,
            DRAWABLE_CONTAINER_HANDLE_SENTINEL,
        ] {
            assert!(!is_registry_paintable(sentinel));
            retain_paintable(sentinel);
            release_paintable(sentinel);
        }
    }

    #[test]
    fn resolve_theme_attributes_reads_a_registered_theme_and_is_total_on_bad_handles() {
        use crate::framework::theme_registry;
        let theme = theme_registry::allocate().expect("allocate theme");
        let attr_a = u32_to_i32(0x7f01_0058);
        let attr_b = u32_to_i32(0x7f01_00a9);
        theme_registry::with_theme(theme, |t| {
            t.attrs.insert(
                attr_a,
                theme_registry::ThemeAttr {
                    type_: 0x12,
                    data: 1,
                    source_package: 0x7f,
                },
            );
        })
        .expect("populate theme");

        let out = resolve_theme_attributes(theme, &[attr_a, attr_b], &ResConfig::default());
        assert_eq!(out.len(), 2);
        assert!(out[0].is_some(), "registered attr resolves");
        assert!(out[1].is_none(), "unset attr is None (→ TYPE_NULL default)");

        let bogus = i64::MAX;
        let out = resolve_theme_attributes(bogus, &[attr_a, attr_b], &ResConfig::default());
        assert_eq!(out, vec![None, None]);

        theme_registry::free(theme).expect("free theme");
    }

    #[test]
    fn resolve_inline_theme_refs_resolves_attribute_values_against_the_theme() {
        use crate::framework::theme_registry;
        let theme = theme_registry::allocate().expect("allocate theme");

        let referenced_attr = 0x7f01_0001u32;
        theme_registry::with_theme(theme, |t| {
            t.attrs.insert(
                u32_to_i32(referenced_attr),
                theme_registry::ThemeAttr {
                    type_: 0x10,
                    data: 42,
                    source_package: 0x7f,
                },
            );
        })
        .expect("populate theme");

        let mut entries = vec![
            Some(TypedEntry {
                value_type: i32::from(TYPE_ATTRIBUTE),
                data: u32_to_i32(referenced_attr),
                resource_id: u32_to_i32(referenced_attr),
                asset_cookie: 0,
            }),
            Some(TypedEntry {
                value_type: 0x1c,
                data: 0x1234_5678,
                resource_id: 0,
                asset_cookie: 0,
            }),
            None,
        ];
        resolve_inline_theme_refs(theme, &mut entries, &ResConfig::default());

        let resolved = entries[0].expect("the ?attr value resolved against the theme");
        assert_eq!(
            resolved.value_type, 0x10,
            "resolved to the theme attr's type"
        );
        assert_eq!(resolved.data, 42, "resolved to the theme attr's data");
        assert_eq!(
            entries[1].expect("concrete slot untouched").data,
            0x1234_5678
        );
        assert!(entries[2].is_none(), "absent slot stays None");

        let mut undefined = vec![Some(TypedEntry {
            value_type: i32::from(TYPE_ATTRIBUTE),
            data: u32_to_i32(0x7f01_9999),
            resource_id: u32_to_i32(0x7f01_9999),
            asset_cookie: 0,
        })];
        resolve_inline_theme_refs(theme, &mut undefined, &ResConfig::default());
        assert_eq!(
            undefined[0].expect("slot present").value_type,
            i32::from(TYPE_ATTRIBUTE),
            "an attr absent from the theme stays an unresolved reference"
        );

        theme_registry::free(theme).expect("free theme");
    }

    #[test]
    fn resolve_xml_attributes_serves_include_android_id_and_never_matches_attr_zero() {
        use crate::apk::axml::{XmlAttribute, XmlDocument, XmlElement, XmlEventKind};
        let include = XmlElement {
            namespace: None,
            name: Some("include".to_string()),
            attributes: vec![
                XmlAttribute {
                    namespace: Some("http://schemas.android.com/apk/res/android".to_string()),
                    name: Some("id".to_string()),
                    name_resource: 0x0101_00d0,
                    value_type: TYPE_REFERENCE,
                    value_data: 0x7f09_027b,
                    value_string: None,
                },
                XmlAttribute {
                    namespace: None,
                    name: Some("layout".to_string()),
                    name_resource: 0,
                    value_type: TYPE_REFERENCE,
                    value_data: 0x7f0c_00a3,
                    value_string: None,
                },
            ],
            line: 0,
        };
        let doc = XmlDocument {
            events: vec![XmlEventKind::StartTag(0), XmlEventKind::EndTag(0)],
            elements: vec![include],
            texts: vec![],
            namespaces: vec![],
            strings: vec![],
        };
        let handle = xml_registry::store(doc).expect("store include block");

        xml_registry::with_block(handle, |b| {
            b.next_event();
        })
        .expect("advance to include");

        let out =
            resolve_xml_attributes(handle, &[u32_to_i32(0x0101_00d0), 0], &ResConfig::default());
        assert_eq!(out.len(), 2);
        let id_entry = out[0].expect("android:id resolves on the include tag");
        assert_eq!(
            id_entry.resource_id,
            u32_to_i32(0x7f09_027b),
            "getResourceId must see the include-tag override id"
        );
        assert_ne!(
            id_entry.value_type, 0,
            "TYPE_NULL would make TypedArray.getResourceId return the 0 default → override dropped"
        );
        assert!(
            out[1].is_none(),
            "requested attr id 0 must never match (the zero-stub failure signature)"
        );

        xml_registry::free(handle).expect("free include block");
    }

    #[test]
    fn context_native_names_and_sigs_match_context_java() {
        assert_eq!(NATIVE_GET_APK_PATH_NAME.to_str(), "native_get_apk_path");
        assert_eq!(NATIVE_GET_APK_PATH_SIG.to_str(), "()Ljava/lang/String;");
        assert_eq!(NATIVE_UPDATE_CONFIG_NAME.to_str(), "native_updateConfig");
        assert_eq!(
            NATIVE_UPDATE_CONFIG_SIG.to_str(),
            "(Landroid/content/res/Configuration;)V"
        );

        assert_eq!(SCREEN_WIDTH_DP_FIELD.to_str(), "screenWidthDp");
        assert_eq!(SCREEN_HEIGHT_DP_FIELD.to_str(), "screenHeightDp");
        assert_eq!(INT_SIG.to_str(), "I");
    }

    #[test]
    fn log_native_name_sig_and_class_match_log_java() {
        assert_eq!(LOG_CLASS.to_str(), "android/util/Log");
        assert_eq!(PRINTLN_NATIVE_NAME.to_str(), "println_native");
        assert_eq!(
            PRINTLN_NATIVE_SIG.to_str(),
            "(IILjava/lang/String;Ljava/lang/String;)I"
        );

        assert_eq!(LOG_ID_MAX, 4);

        assert_eq!(LOG_PRIORITY_VERBOSE, 2);
        assert_eq!(LOG_PRIORITY_DEBUG, 3);
        assert_eq!(LOG_PRIORITY_INFO, 4);
        assert_eq!(LOG_PRIORITY_WARN, 5);
        assert_eq!(LOG_PRIORITY_ERROR, 6);
        assert_eq!(LOG_PRIORITY_ASSERT, 7);
    }

    fn forwarded_java_log(priority: jint, message: &str) -> String {
        crate::diagnostics::captured_log_lines(|| {
            fake_jvm::with_env(|env| {
                let tag = env.new_string("RobloxWeb").expect("tag");
                let msg = env.new_string(message).expect("message");
                let written = println_native(
                    fake_jvm::native_env(),
                    JClass::null(),
                    0,
                    priority,
                    tag,
                    msg,
                );
                assert_eq!(usize::try_from(written), Ok(message.len()));
            });
        })
    }

    #[test]
    fn forwarded_java_log_lines_keep_only_scheme_and_host_of_urls_at_every_priority() {
        for priority in [
            LOG_PRIORITY_VERBOSE,
            LOG_PRIORITY_DEBUG,
            LOG_PRIORITY_INFO,
            LOG_PRIORITY_WARN,
            LOG_PRIORITY_ERROR,
            LOG_PRIORITY_ASSERT,
        ] {
            let log = forwarded_java_log(
                priority,
                "loading https://www.roblox.com/login?token=abc&x=1 now",
            );
            assert!(
                log.contains(
                    "android.util.Log: loading https://www.roblox.com now tag=\"RobloxWeb\"\n"
                ),
                "priority {priority}: {log}"
            );
            assert!(
                !log.contains("token=") && !log.contains("abc"),
                "priority {priority}: {log}"
            );
        }
    }

    #[test]
    fn forwarded_java_log_lines_without_urls_are_unchanged() {
        let message = "Roblox ready: 100% of 3 tasks, path=/data/user/0 key:value";
        let log = forwarded_java_log(LOG_PRIORITY_INFO, message);
        assert!(
            log.contains(&format!(
                " INFO android.util.Log: {message} tag=\"RobloxWeb\"\n"
            )),
            "{log}"
        );
    }

    #[test]
    fn asset_manager_init_name_sig_and_class_match_asset_manager_java() {
        assert_eq!(
            ASSET_MANAGER_CLASS.to_str(),
            "android/content/res/AssetManager"
        );
        assert_eq!(ASSET_MANAGER_INIT_NAME.to_str(), "init");
        assert_eq!(ASSET_MANAGER_INIT_SIG.to_str(), "(I)V");

        assert_eq!(
            ASSET_MANAGER_SET_APK_ASSETS_NAME.to_str(),
            "native_setApkAssets"
        );
        assert_eq!(
            ASSET_MANAGER_SET_APK_ASSETS_SIG.to_str(),
            "([Ljava/lang/Object;I)V"
        );

        assert_eq!(
            ASSET_MANAGER_SET_CONFIGURATION_NAME.to_str(),
            "setConfiguration"
        );
        assert_eq!(
            ASSET_MANAGER_SET_CONFIGURATION_SIG.to_str(),
            "(IILjava/lang/String;IIIIIIIIIIIIII)V"
        );

        assert_eq!(
            ASSET_MANAGER_OPEN_XML_ASSET_NAME.to_str(),
            "openXmlAssetNative"
        );
        assert_eq!(
            ASSET_MANAGER_OPEN_XML_ASSET_SIG.to_str(),
            "(ILjava/lang/String;)J"
        );

        assert_eq!(
            ASSET_MANAGER_RETRIEVE_ATTRIBUTES_NAME.to_str(),
            "retrieveAttributes"
        );
        assert_eq!(ASSET_MANAGER_RETRIEVE_ATTRIBUTES_SIG.to_str(), "(J[IIJJ)Z");

        assert_eq!(ASSET_MANAGER_NEW_THEME_NAME.to_str(), "newTheme");
        assert_eq!(ASSET_MANAGER_NEW_THEME_SIG.to_str(), "()J");

        assert_eq!(
            ASSET_MANAGER_APPLY_THEME_STYLE_NAME.to_str(),
            "applyThemeStyle"
        );
        assert_eq!(ASSET_MANAGER_APPLY_THEME_STYLE_SIG.to_str(), "(JIZ)V");

        assert_eq!(ASSET_MANAGER_COPY_THEME_NAME.to_str(), "copyTheme");
        assert_eq!(ASSET_MANAGER_COPY_THEME_SIG.to_str(), "(JJ)V");

        assert_eq!(ASSET_MANAGER_APPLY_STYLE_NAME.to_str(), "applyStyle");
        assert_eq!(ASSET_MANAGER_APPLY_STYLE_SIG.to_str(), "(JJII[IIJJ)V");

        assert_eq!(
            ASSET_MANAGER_GET_RESOURCE_NAME_NAME.to_str(),
            "getResourceName"
        );
        assert_eq!(
            ASSET_MANAGER_GET_RESOURCE_NAME_SIG.to_str(),
            "(I)Ljava/lang/String;"
        );

        assert_eq!(
            ASSET_MANAGER_LOAD_RESOURCE_VALUE_NAME.to_str(),
            "loadResourceValue"
        );
        assert_eq!(
            ASSET_MANAGER_LOAD_RESOURCE_VALUE_SIG.to_str(),
            "(ISLandroid/util/TypedValue;Z)I"
        );

        assert_eq!(
            ASSET_MANAGER_LOAD_THEME_ATTRIBUTE_VALUE_NAME.to_str(),
            "loadThemeAttributeValue"
        );
        assert_eq!(
            ASSET_MANAGER_LOAD_THEME_ATTRIBUTE_VALUE_SIG.to_str(),
            "(JILandroid/util/TypedValue;Z)I"
        );

        assert_eq!(
            ASSET_MANAGER_GET_POOLED_STRING_NAME.to_str(),
            "getPooledString"
        );
        assert_eq!(
            ASSET_MANAGER_GET_POOLED_STRING_SIG.to_str(),
            "(II)Ljava/lang/CharSequence;"
        );
        assert_eq!(CHAR_SEQUENCE_SIG.to_str(), "Ljava/lang/CharSequence;");
        assert_eq!(RES_VALUE_TYPE_STRING, 0x03);
        assert_eq!(ECLIPSE_ASSET_COOKIE, 1);

        assert_eq!(ARSC_APP_COOKIE, 1);
        assert_eq!(ARSC_FRAMEWORK_COOKIE, 2);
        assert_eq!(arsc_cookie_for(0x7f08_0173), ARSC_APP_COOKIE);
        assert_eq!(arsc_cookie_for(0x0108_0000), ARSC_FRAMEWORK_COOKIE);
        assert_eq!(arsc_cookie_for_package(0x7f), ARSC_APP_COOKIE);
        assert_eq!(arsc_cookie_for_package(0x01), ARSC_FRAMEWORK_COOKIE);

        assert_eq!(STYLE_NUM_ENTRIES, 7);
        assert_eq!(STYLE_TYPE, 0);
        assert_eq!(STYLE_DATA, 1);
        assert_eq!(STYLE_ASSET_COOKIE, 2);
        assert_eq!(STYLE_RESOURCE_ID, 3);
        assert_eq!(TYPE_NULL, 0);
        assert_eq!(TYPE_REFERENCE, 0x01);
        assert_eq!(TYPE_ATTRIBUTE, 0x02);
        assert_eq!(TYPE_STRING, 0x03);
        assert_eq!(XML_BLOCK_COOKIE, -1);
    }

    #[test]
    fn fill_typed_array_writes_exact_bounds_values_and_indices() {
        let entries = [
            Some(TypedEntry {
                value_type: i32::from(TYPE_STRING),
                data: 0x18,
                resource_id: 0,
                asset_cookie: XML_BLOCK_COOKIE,
            }),
            None,
            Some(TypedEntry {
                value_type: i32::from(TYPE_REFERENCE),
                data: 0x7f03_0000,
                resource_id: 0x7f03_0000,
                asset_cookie: 0,
            }),
            None,
        ];
        let n = entries.len();
        let vals_len = n * STYLE_NUM_ENTRIES;
        let idx_len = n + 1;

        let mut values = vec![-1i32; vals_len + 2];
        let mut indices = vec![-1i32; idx_len + 2];

        let v_ptr = values[1..1 + vals_len].as_mut_ptr() as jlong;
        let i_ptr = indices[1..1 + idx_len].as_mut_ptr() as jlong;
        fill_typed_array(v_ptr, i_ptr, &entries);

        assert_eq!(values[0], -1, "outValues underflow guard");
        assert_eq!(values[vals_len + 1], -1, "outValues overflow guard");
        assert_eq!(indices[0], -1, "outIndices underflow guard");
        assert_eq!(indices[idx_len + 1], -1, "outIndices overflow guard");

        for (attr, e) in [(0usize, &entries[0]), (2usize, &entries[2])] {
            let win = 1 + attr * STYLE_NUM_ENTRIES;
            let e = e.unwrap();
            assert_eq!(
                values[win..win + STYLE_NUM_ENTRIES],
                [e.value_type, e.data, e.asset_cookie, e.resource_id, 0, 0, 0],
                "a found entry fills its whole window"
            );
        }

        for attr in [1usize, 3usize] {
            let win = 1 + attr * STYLE_NUM_ENTRIES;
            assert_eq!(
                values[win..win + STYLE_NUM_ENTRIES],
                [TYPE_NULL, DATA_NULL_UNDEFINED, NO_ASSET_COOKIE, 0, 0, 0, 0],
                "an absent entry is TYPE_NULL/DATA_NULL_UNDEFINED with no cookie"
            );
        }

        assert_eq!(indices[1], 2, "outIndices[0] = number found");
        assert_eq!(indices[2], 0, "first found at request position 0");
        assert_eq!(indices[3], 2, "second found at request position 2");
        assert_eq!(indices[1 + 3], -1, "outIndices beyond count untouched");
    }

    #[test]
    fn typed_array_get_index_addresses_the_found_attribute_window() {
        let found = |value_type: u8, data: i32| {
            Some(TypedEntry {
                value_type: i32::from(value_type),
                data,
                resource_id: 0,
                asset_cookie: XML_BLOCK_COOKIE,
            })
        };
        let entries = [
            None,
            found(TYPE_STRING, 0x11),
            None,
            found(TYPE_REFERENCE, 0x7f03_0001),
        ];
        let n = entries.len();
        let mut values = vec![0x5a5a_5a5ai32; n * STYLE_NUM_ENTRIES];
        let mut indices = vec![0i32; n + 1];
        fill_typed_array(
            values.as_mut_ptr() as jlong,
            indices.as_mut_ptr() as jlong,
            &entries,
        );

        let count = usize::try_from(indices[0]).expect("count");
        assert_eq!(count, 2);
        for at in 0..count {
            let index = usize::try_from(indices[1 + at]).expect("index");
            assert!(
                index < n,
                "getIndex({at}) = {index} is outside the {n} attrs"
            );
            let expected = entries[index].expect("getIndex names a found attribute");
            assert_eq!(
                values[index * STYLE_NUM_ENTRIES + STYLE_TYPE],
                expected.value_type
            );
            assert_eq!(
                values[index * STYLE_NUM_ENTRIES + STYLE_DATA],
                expected.data
            );
        }
    }

    #[test]
    fn fill_typed_array_reference_resource_id_is_at_style_resource_id_slot() {
        let id = 0x7f03_0000i32;
        let entries = [Some(TypedEntry {
            value_type: i32::from(TYPE_REFERENCE),
            data: id,
            resource_id: id,
            asset_cookie: 0,
        })];
        let mut values = vec![0i32; STYLE_NUM_ENTRIES];
        let v_ptr = values.as_mut_ptr() as jlong;
        fill_typed_array(v_ptr, 0, &entries);
        assert_eq!(values[STYLE_TYPE], i32::from(TYPE_REFERENCE));
        assert_eq!(
            values[STYLE_RESOURCE_ID], id,
            "getResourceId reads the referenced id from STYLE_RESOURCE_ID"
        );
    }

    #[test]
    fn fill_typed_array_null_pointers_are_a_no_op() {
        let entries = [Some(TypedEntry {
            value_type: i32::from(TYPE_STRING),
            data: 1,
            resource_id: 0,
            asset_cookie: XML_BLOCK_COOKIE,
        })];
        fill_typed_array(0, 0, &entries);
    }

    #[test]
    fn fill_typed_array_zero_attrs_writes_only_changed_count() {
        let mut indices = [-1i32; 3];
        let i_ptr = indices[1..2].as_mut_ptr() as jlong;
        fill_typed_array(0, i_ptr, &[]);
        assert_eq!(indices[0], -1, "underflow guard untouched");
        assert_eq!(indices[1], 0, "outIndices[0] = 0 with zero attrs");
        assert_eq!(indices[2], -1, "overflow guard untouched");
    }

    #[test]
    fn u32_to_i32_preserves_all_bits() {
        for &v in &[0u32, 1, 0x7fff_ffff, 0x8000_0000, 0xffff_ffff, 0x0101_0003] {
            assert_eq!(u32_to_i32(v).to_ne_bytes(), v.to_ne_bytes());
        }
    }

    #[test]
    fn xml_block_native_names_sigs_and_class_match_art_reported() {
        assert_eq!(XML_BLOCK_CLASS.to_str(), "android/content/res/XmlBlock");
        assert_eq!(
            XML_BLOCK_CREATE_PARSE_STATE_NAME.to_str(),
            "nativeCreateParseState"
        );
        assert_eq!(XML_BLOCK_CREATE_PARSE_STATE_SIG.to_str(), "(J)J");
        assert_eq!(XML_BLOCK_NEXT_NAME.to_str(), "nativeNext");
        assert_eq!(XML_BLOCK_NEXT_SIG.to_str(), "(J)I");
        assert_eq!(
            XML_BLOCK_DESTROY_PARSE_STATE_NAME.to_str(),
            "nativeDestroyParseState"
        );
        assert_eq!(XML_BLOCK_DESTROY_PARSE_STATE_SIG.to_str(), "(J)V");
        assert_eq!(XML_BLOCK_GET_NAME_NAME.to_str(), "nativeGetName");
        assert_eq!(XML_BLOCK_GET_NAME_SIG.to_str(), "(J)Ljava/lang/String;");
        assert_eq!(XML_BLOCK_DESTROY_NAME.to_str(), "nativeDestroy");
        assert_eq!(XML_BLOCK_DESTROY_SIG.to_str(), "(J)V");
        assert_eq!(
            XML_BLOCK_GET_ATTR_INDEX_NAME.to_str(),
            "nativeGetAttributeIndex"
        );
        assert_eq!(
            XML_BLOCK_GET_ATTR_INDEX_SIG.to_str(),
            "(JLjava/lang/String;Ljava/lang/String;)I"
        );
        assert_eq!(
            XML_BLOCK_GET_ATTR_STRING_VALUE_NAME.to_str(),
            "nativeGetAttributeStringValue"
        );
        assert_eq!(
            XML_BLOCK_GET_ATTR_STRING_VALUE_SIG.to_str(),
            "(JI)Ljava/lang/String;"
        );

        assert_eq!(
            XML_BLOCK_GET_ATTR_COUNT_NAME.to_str(),
            "nativeGetAttributeCount"
        );
        assert_eq!(XML_BLOCK_GET_ATTR_COUNT_SIG.to_str(), "(J)I");

        assert_eq!(
            XML_BLOCK_GET_ATTR_RESOURCE_NAME.to_str(),
            "nativeGetAttributeResource"
        );
        assert_eq!(XML_BLOCK_GET_ATTR_RESOURCE_SIG.to_str(), "(JI)I");

        assert_eq!(
            XML_BLOCK_GET_ATTR_DATA_TYPE_NAME.to_str(),
            "nativeGetAttributeDataType"
        );
        assert_eq!(XML_BLOCK_GET_ATTR_DATA_TYPE_SIG.to_str(), "(JI)I");
        assert_eq!(
            XML_BLOCK_GET_ATTR_DATA_NAME.to_str(),
            "nativeGetAttributeData"
        );
        assert_eq!(XML_BLOCK_GET_ATTR_DATA_SIG.to_str(), "(JI)I");

        assert_eq!(
            XML_BLOCK_GET_LINE_NUMBER_NAME.to_str(),
            "nativeGetLineNumber"
        );
        assert_eq!(XML_BLOCK_GET_LINE_NUMBER_SIG.to_str(), "(J)I");
        assert_eq!(
            XML_BLOCK_GET_POOLED_STRING_NAME.to_str(),
            "nativeGetPooledString"
        );
        assert_eq!(
            XML_BLOCK_GET_POOLED_STRING_SIG.to_str(),
            "(JI)Ljava/lang/String;"
        );
        assert_eq!(XML_LINE_UNKNOWN, -1);

        assert_eq!(XML_EVENT_END_DOCUMENT, 1);
        assert_eq!(XML_EVENT_START_TAG, 2);
        assert_eq!(XML_EVENT_END_TAG, 3);
        assert_eq!(XML_EVENT_TEXT, 4);
        assert_eq!(XML_ATTR_NOT_FOUND, -1);
    }

    #[test]
    fn environment_native_name_sig_and_class_match_environment_java() {
        assert_eq!(ENVIRONMENT_CLASS.to_str(), "android/os/Environment");
        assert_eq!(GET_APP_DATA_DIR_NAME.to_str(), "native_get_app_data_dir");
        assert_eq!(GET_APP_DATA_DIR_SIG.to_str(), "()Ljava/lang/String;");
    }

    #[test]
    fn system_clock_native_name_sig_and_class_match_system_clock_java() {
        assert_eq!(SYSTEM_CLOCK_CLASS.to_str(), "android/os/SystemClock");
        assert_eq!(ELAPSED_REALTIME_NAME.to_str(), "elapsedRealtime");
        assert_eq!(ELAPSED_REALTIME_SIG.to_str(), "()J");
        assert_eq!(ELAPSED_REALTIME_NANOS_NAME.to_str(), "elapsedRealtimeNanos");
        assert_eq!(ELAPSED_REALTIME_NANOS_SIG.to_str(), "()J");

        assert_eq!(UPTIME_MILLIS_NAME.to_str(), "uptimeMillis");
        assert_eq!(UPTIME_MILLIS_SIG.to_str(), "()J");
    }

    #[test]
    fn runtime_native_load_name_sig_and_class_match_art() {
        assert_eq!(RUNTIME_CLASS.to_str(), "java/lang/Runtime");
        assert_eq!(NATIVE_LOAD_NAME.to_str(), "nativeLoad");
        assert_eq!(
            NATIVE_LOAD_SIG.to_str(),
            "(Ljava/lang/String;Ljava/lang/ClassLoader;Ljava/lang/Class;)Ljava/lang/String;"
        );

        assert!(ART_LOAD_NATIVE_LIBRARY_SYMBOL.starts_with(b"_ZN3art9JavaVMExt17LoadNativeLibrary"));
        assert_eq!(*ART_LOAD_NATIVE_LIBRARY_SYMBOL.last().unwrap(), 0u8);
    }

    #[test]
    fn soname_from_load_path_returns_the_basename() {
        assert_eq!(
            soname_from_load_path("/home/u/.cache/eclipse/native-libs/libzstd-jni-1.5.7-6.so"),
            "libzstd-jni-1.5.7-6.so"
        );
        assert_eq!(soname_from_load_path("libroblox.so"), "libroblox.so");
        assert_eq!(
            soname_from_load_path("/usr/lib/libwolfssljni.so"),
            "libwolfssljni.so"
        );
        assert_eq!(soname_from_load_path(""), "");
        assert_eq!(soname_from_load_path("/a/b/"), "");
    }

    #[test]
    fn asset_manager_get_resource_package_name_name_sig_match_art_reported() {
        assert_eq!(
            ASSET_MANAGER_GET_RESOURCE_PACKAGE_NAME_NAME.to_str(),
            "getResourcePackageName"
        );
        assert_eq!(
            ASSET_MANAGER_GET_RESOURCE_PACKAGE_NAME_SIG.to_str(),
            "(I)Ljava/lang/String;"
        );
    }

    #[test]
    fn asset_manager_get_resource_identifier_name_sig_match_art_reported() {
        assert_eq!(
            ASSET_MANAGER_GET_RESOURCE_IDENTIFIER_NAME.to_str(),
            "getResourceIdentifier"
        );
        assert_eq!(
            ASSET_MANAGER_GET_RESOURCE_IDENTIFIER_SIG.to_str(),
            "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)I"
        );
    }

    #[test]
    fn asset_manager_stream_native_names_and_sigs_are_the_classic_aosp_set() {
        assert_eq!(ASSET_MANAGER_OPEN_ASSET_NAME.to_str(), "openAsset");
        assert_eq!(
            ASSET_MANAGER_OPEN_ASSET_SIG.to_str(),
            "(Ljava/lang/String;I)J"
        );
        assert_eq!(ASSET_MANAGER_READ_ASSET_NAME.to_str(), "readAsset");

        assert_eq!(ASSET_MANAGER_READ_ASSET_SIG.to_str(), "(J[BJJ)I");

        assert_eq!(ASSET_MANAGER_READ_ASSET_CHAR_NAME.to_str(), "readAssetChar");
        assert_eq!(ASSET_MANAGER_READ_ASSET_CHAR_SIG.to_str(), "(J)I");
        assert_eq!(ASSET_MANAGER_SEEK_ASSET_NAME.to_str(), "seekAsset");
        assert_eq!(ASSET_MANAGER_SEEK_ASSET_SIG.to_str(), "(JJI)J");
        assert_eq!(
            ASSET_MANAGER_GET_ASSET_LENGTH_NAME.to_str(),
            "getAssetLength"
        );
        assert_eq!(ASSET_MANAGER_GET_ASSET_LENGTH_SIG.to_str(), "(J)J");
        assert_eq!(
            ASSET_MANAGER_GET_ASSET_REMAINING_LENGTH_NAME.to_str(),
            "getAssetRemainingLength"
        );
        assert_eq!(ASSET_MANAGER_DESTROY_ASSET_NAME.to_str(), "destroyAsset");
        assert_eq!(ASSET_MANAGER_DESTROY_ASSET_SIG.to_str(), "(J)V");

        assert_eq!(ASSET_MANAGER_OPEN_ASSET_FD_NAME.to_str(), "openAssetFd");
        assert_eq!(
            ASSET_MANAGER_OPEN_ASSET_FD_SIG.to_str(),
            "(Ljava/lang/String;I[J[J)I"
        );
    }

    #[test]
    fn asset_fd_for_serves_stored_entries_and_refuses_absent_and_compressed() {
        use std::io::{Read, Seek, SeekFrom, Write};
        const PAYLOAD: &[u8] = b"baseline-profile-bytes";

        let mut zip_bytes = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut zip_bytes));
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            writer
                .start_file("assets/dexopt/baseline.prof", stored)
                .expect("start stored");
            writer.write_all(PAYLOAD).expect("write stored");
            let deflated = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            writer
                .start_file("assets/compressed.bin", deflated)
                .expect("start deflated");
            writer.write_all(PAYLOAD).expect("write deflated");
            writer.finish().expect("finish zip");
        }
        let mut path = std::env::temp_dir();
        path.push(format!(
            "eclipse-openassetfd-test-{:?}.apk",
            std::thread::current().id()
        ));
        std::fs::write(&path, &zip_bytes).expect("write fixture apk");
        let apk_path = path.to_str().expect("utf-8 temp path");

        for name in ["assets/dexopt/baseline.prof", "dexopt/baseline.prof"] {
            let (fd, offset, length) = asset_fd_for(apk_path, name).expect("stored asset fd");
            assert!(fd >= 0, "a real owned fd");
            assert_eq!(length, PAYLOAD.len() as u64);

            let mut file: std::fs::File = unsafe { std::os::fd::FromRawFd::from_raw_fd(fd) };
            file.seek(SeekFrom::Start(offset)).expect("seek to offset");
            let mut got = vec![0u8; PAYLOAD.len()];
            file.read_exact(&mut got).expect("read asset bytes");
            assert_eq!(got, PAYLOAD, "the (fd, offset, length) window IS the asset");
        }

        assert!(matches!(
            asset_fd_for(apk_path, "assets/absent.bin"),
            Err(AssetFdError::Apk(crate::apk::ApkError::EntryMissing(_)))
        ));

        assert!(matches!(
            asset_fd_for(apk_path, "assets/compressed.bin"),
            Err(AssetFdError::Compressed)
        ));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn atl_read_asset_return_maps_eof_to_zero_and_errors_negative() {
        assert_eq!(atl_read_asset_return(&Ok(vec![7u8; 3])), 3);
        assert_eq!(
            atl_read_asset_return(&Ok(Vec::new())),
            0,
            "EOF must be 0 (Java maps it to -1); any negative throws IOException"
        );
        assert_eq!(
            atl_read_asset_return(&Err(asset_registry::AssetRegistryError::StaleHandle)),
            -1,
            "only a genuine error is negative (→ the designed IOException)"
        );
    }

    #[test]
    fn atl_seek_whence_translation_matches_asset_input_stream_callers() {
        assert_eq!(atl_seek_whence_to_lseek(-1), 0, "whence < 0 is SET");
        assert_eq!(atl_seek_whence_to_lseek(0), 1, "whence 0 is CUR");
        assert_eq!(atl_seek_whence_to_lseek(1), 2, "whence > 0 is END");
    }

    #[test]
    fn root_relative_res_entry_serves_open_read_seek_and_length_via_the_shared_rule() {
        use std::io::{Read, Seek, SeekFrom, Write};

        const PNG: &[u8] = &[
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, b'I', b'H',
            b'D', b'R', 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x03, 0x08, 0x06, 0x00, 0x00,
            0x00,
        ];
        const CFG: &[u8] = b"cfg-bytes";
        let mut zip_bytes = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut zip_bytes));
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            writer
                .start_file("res/drawable-hdpi-v4/roblox_logo.png", stored)
                .expect("start res entry");
            writer.write_all(PNG).expect("write res entry");
            writer
                .start_file("assets/config.txt", stored)
                .expect("start assets entry");
            writer.write_all(CFG).expect("write assets entry");
            writer.finish().expect("finish zip");
        }
        let mut path = std::env::temp_dir();
        path.push(format!(
            "eclipse-root-relative-asset-test-{:?}.apk",
            std::thread::current().id()
        ));
        std::fs::write(&path, &zip_bytes).expect("write fixture apk");
        let apk_path = path.to_str().expect("utf-8 temp path");

        let bytes = read_asset_bytes_from(apk_path, "res/drawable-hdpi-v4/roblox_logo.png")
            .expect("root-relative res entry must open");
        assert_eq!(bytes, PNG);
        assert_eq!(
            read_asset_bytes_from(apk_path, "config.txt").as_deref(),
            Some(CFG)
        );
        assert_eq!(
            read_asset_bytes_from(apk_path, "assets/config.txt").as_deref(),
            Some(CFG)
        );
        assert!(read_asset_bytes_from(apk_path, "assets/roblox_logo.png").is_none());

        let handle = asset_registry::store(bytes).expect("store opened asset");
        assert_eq!(
            asset_registry::with_stream(handle, |s| s.len()),
            Ok(PNG.len())
        );
        let read_chunk = |want: usize| {
            asset_registry::with_stream(handle, |s| {
                let mut buf = vec![0u8; want];
                let n = s.read(&mut buf);
                buf.truncate(n);
                buf
            })
        };
        let full = read_chunk(PNG.len() + 8);
        assert_eq!(
            atl_read_asset_return(&full),
            i32::try_from(PNG.len()).expect("fits"),
            "a data read returns the byte count"
        );
        assert_eq!(full.as_deref(), Ok(PNG));
        assert_eq!(
            atl_read_asset_return(&read_chunk(16)),
            0,
            "the terminal EOF read must map to 0 — -1 makes ATL's readAsset_internal throw \
             IOException (the challenge4 splash fatal)"
        );

        assert_eq!(
            asset_registry::with_stream(handle, |s| s.seek(0, atl_seek_whence_to_lseek(-1))),
            Ok(0)
        );
        assert_eq!(
            asset_registry::with_stream(handle, |s| s.remaining()),
            Ok(PNG.len())
        );
        drop(read_chunk(4));
        assert_eq!(
            asset_registry::with_stream(handle, |s| s.seek(0, atl_seek_whence_to_lseek(0))),
            Ok(4),
            "mark()'s whence-0 seek reports the current position"
        );
        assert_eq!(
            asset_registry::with_stream(handle, |s| s.remaining()),
            Ok(PNG.len() - 4),
            "the whence-0 position query must not rewind the stream"
        );
        asset_registry::free(handle).expect("free");

        let (fd, offset, length) = asset_fd_for(apk_path, "res/drawable-hdpi-v4/roblox_logo.png")
            .expect("root-relative res entry must be fd-servable");
        assert_eq!(length, PNG.len() as u64);

        let mut file: std::fs::File = unsafe { std::os::fd::FromRawFd::from_raw_fd(fd) };
        file.seek(SeekFrom::Start(offset)).expect("seek to offset");
        let mut got = vec![0u8; PNG.len()];
        file.read_exact(&mut got).expect("read entry bytes");
        assert_eq!(got, PNG);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn resolve_resource_identifier_parses_name_forms_and_returns_zero_when_unresolvable() {
        assert_eq!(resolve_resource_identifier("", "string", "com.x"), 0);
        assert_eq!(resolve_resource_identifier("foo", "", ""), 0);
        assert_eq!(resolve_resource_identifier("type/", "", ""), 0);
    }

    #[test]
    fn message_queue_native_name_sig_and_class_match_art_reported() {
        assert_eq!(MESSAGE_QUEUE_CLASS.to_str(), "android/os/MessageQueue");
        assert_eq!(MESSAGE_QUEUE_NATIVE_INIT_NAME.to_str(), "nativeInit");
        assert_eq!(MESSAGE_QUEUE_NATIVE_INIT_SIG.to_str(), "()J");
        assert_eq!(MESSAGE_QUEUE_NATIVE_DESTROY_NAME.to_str(), "nativeDestroy");
        assert_eq!(MESSAGE_QUEUE_NATIVE_DESTROY_SIG.to_str(), "(J)V");
        assert_eq!(
            MESSAGE_QUEUE_NATIVE_IS_IDLING_NAME.to_str(),
            "nativeIsIdling"
        );
        assert_eq!(MESSAGE_QUEUE_NATIVE_IS_IDLING_SIG.to_str(), "(J)Z");
        assert_eq!(
            MESSAGE_QUEUE_NATIVE_POLL_ONCE_NAME.to_str(),
            "nativePollOnce"
        );
        assert_eq!(MESSAGE_QUEUE_NATIVE_POLL_ONCE_SIG.to_str(), "(JI)Z");
        assert_eq!(MESSAGE_QUEUE_NATIVE_WAKE_NAME.to_str(), "nativeWake");
        assert_eq!(MESSAGE_QUEUE_NATIVE_WAKE_SIG.to_str(), "(J)V");

        assert_eq!(LOOPER_CLASS.to_str(), "android/os/Looper");
    }

    #[test]
    fn main_looper_poll_yield_table_matches_atl_next_contract() {
        assert!(
            !main_looper_poll_should_yield(0),
            "timeout==0 must pull, not yield"
        );
        assert!(
            main_looper_poll_should_yield(-1),
            "timeout==-1 (empty) must yield"
        );
        assert!(
            main_looper_poll_should_yield(1),
            "timeout>0 (delayed) must yield"
        );
        assert!(
            main_looper_poll_should_yield(i32::MAX),
            "large delay must yield"
        );
    }

    #[test]
    fn main_looper_due_maps_to_a_wake_deadline() {
        let now = Instant::now();
        let frame = now + std::time::Duration::from_millis(16);

        assert_eq!(MainLooperDue::Now.deadline(now), Some(now));
        assert_eq!(MainLooperDue::At(frame).deadline(now), Some(frame));
        assert_eq!(MainLooperDue::WhenWoken.deadline(now), None);
    }

    #[test]
    fn sensor_manager_native_name_sig_and_class_match_art_reported() {
        assert_eq!(
            SENSOR_MANAGER_CLASS.to_str(),
            "android/hardware/SensorManager"
        );
        assert_eq!(
            SENSOR_MANAGER_REGISTER_NAME.to_str(),
            "register_accelerometer_listener_native"
        );
        assert_eq!(
            SENSOR_MANAGER_REGISTER_SIG.to_str(),
            "(Landroid/hardware/SensorEventListener;Landroid/hardware/Sensor;I)V"
        );
    }

    #[test]
    fn vibrator_native_names_sigs_and_class_match_vibrator_java() {
        assert_eq!(VIBRATOR_CLASS.to_str(), "android/os/Vibrator");
        assert_eq!(
            VIBRATOR_NATIVE_CONSTRUCTOR_NAME.to_str(),
            "native_constructor"
        );
        assert_eq!(VIBRATOR_NATIVE_CONSTRUCTOR_SIG.to_str(), "()I");
        assert_eq!(VIBRATOR_NATIVE_VIBRATE_NAME.to_str(), "native_vibrate");
        assert_eq!(VIBRATOR_NATIVE_VIBRATE_SIG.to_str(), "(IJ)V");
    }

    #[test]
    fn activity_native_names_sigs_and_class_match_api_impl_dex() {
        assert_eq!(ACTIVITY_CLASS.to_str(), "android/app/Activity");
        assert_eq!(
            ACTIVITY_NATIVE_START_ACTIVITY_NAME.to_str(),
            "nativeStartActivity"
        );
        assert_eq!(
            ACTIVITY_NATIVE_START_ACTIVITY_SIG.to_str(),
            "(Landroid/app/Activity;)V"
        );
        assert_eq!(ACTIVITY_NATIVE_FINISH_NAME.to_str(), "nativeFinish");
        assert_eq!(ACTIVITY_NATIVE_FINISH_SIG.to_str(), "(J)V");
        assert_eq!(
            ACTIVITY_NATIVE_RESUME_ACTIVITY_NAME.to_str(),
            "nativeResumeActivity"
        );
        assert_eq!(
            ACTIVITY_NATIVE_RESUME_ACTIVITY_SIG.to_str(),
            "(Ljava/lang/Class;Landroid/content/Intent;)Z"
        );
        assert_eq!(
            ACTIVITY_IS_IN_MULTI_WINDOW_MODE_NAME.to_str(),
            "isInMultiWindowMode"
        );
        assert_eq!(ACTIVITY_IS_IN_MULTI_WINDOW_MODE_SIG.to_str(), "()Z");
        assert_eq!(ACTIVITY_IS_TASK_ROOT_NAME.to_str(), "isTaskRoot");
        assert_eq!(ACTIVITY_IS_TASK_ROOT_SIG.to_str(), "()Z");
    }

    #[test]
    fn external_intent_natives_are_bound_with_the_api_impl_signatures() {
        fake_jvm::with_env(external_intents::register_natives).expect("register the natives");
        for (class, name, sig) in [
            (
                "android/app/Activity",
                "nativeOpenURI",
                "(Ljava/lang/String;)V",
            ),
            (
                "android/content/Context",
                "nativeShareFile",
                "(Ljava/lang/String;I)V",
            ),
        ] {
            assert!(
                fake_jvm::registered_native(class, name, sig).is_some(),
                "{class}.{name}{sig} is not bound"
            );
        }
    }

    #[test]
    fn process_native_name_sig_and_class_match_api_impl_dex() {
        assert_eq!(PROCESS_CLASS.to_str(), "android/os/Process");
        assert_eq!(
            PROCESS_GET_ELAPSED_CPU_TIME_NAME.to_str(),
            "getElapsedCpuTime"
        );
        assert_eq!(PROCESS_GET_ELAPSED_CPU_TIME_SIG.to_str(), "()J");
    }

    #[test]
    fn process_is_64_bit_is_registered_and_reports_the_host_word_size() {
        fake_jvm::with_env(register_process_natives).expect("register Process");
        let native = fake_jvm::registered_native("android/os/Process", "is64Bit", "()Z")
            .expect("Process.is64Bit is bound");
        let is_64_bit: extern "system" fn(EnvUnowned<'static>, JClass<'static>) -> jboolean =
            unsafe { std::mem::transmute(native) };
        assert!(is_64_bit(fake_jvm::native_env(), JClass::null()));
    }

    #[test]
    fn process_get_thread_priority_reads_the_nice_value_and_rejects_missing_threads() {
        fake_jvm::with_env(register_process_natives).expect("register Process");
        let native = fake_jvm::registered_native("android/os/Process", "getThreadPriority", "(I)I")
            .expect("Process.getThreadPriority is bound");
        let get_thread_priority: extern "system" fn(
            EnvUnowned<'static>,
            JClass<'static>,
            jint,
        ) -> jint = unsafe { std::mem::transmute(native) };

        let tid = unsafe { libc::gettid() };
        let expected = unsafe { libc::getpriority(libc::PRIO_PROCESS, tid as libc::id_t) };
        assert_eq!(
            get_thread_priority(fake_jvm::native_env(), JClass::null(), tid),
            expected
        );
        assert_eq!(fake_jvm::take_exception(), None);

        get_thread_priority(fake_jvm::native_env(), JClass::null(), i32::MAX);
        assert_eq!(
            fake_jvm::take_exception().as_deref(),
            Some("java/lang/IllegalArgumentException")
        );
    }

    #[test]
    fn system_clock_current_thread_time_millis_is_the_thread_cpu_clock() {
        fake_jvm::with_env(register_system_clock_natives).expect("register SystemClock");
        let native =
            fake_jvm::registered_native("android/os/SystemClock", "currentThreadTimeMillis", "()J")
                .expect("SystemClock.currentThreadTimeMillis is bound");
        let current_thread_time_millis: extern "system" fn(
            EnvUnowned<'static>,
            JClass<'static>,
        ) -> jlong = unsafe { std::mem::transmute(native) };

        let before = cpu_clock_millis(libc::CLOCK_THREAD_CPUTIME_ID);
        let reported = current_thread_time_millis(fake_jvm::native_env(), JClass::null());
        let after = cpu_clock_millis(libc::CLOCK_THREAD_CPUTIME_ID);
        assert!(
            (before..=after).contains(&reported),
            "{before} <= {reported} <= {after}"
        );
    }

    #[test]
    fn engine_preload_binds_every_framework_native_jni_onload_reaches() {
        fake_jvm::with_env(register_jni_onload_natives).expect("register natives");
        for (class, name, sig) in [
            (
                "android/util/Log",
                "println_native",
                "(IILjava/lang/String;Ljava/lang/String;)I",
            ),
            ("android/os/Process", "getElapsedCpuTime", "()J"),
            ("android/os/SystemClock", "elapsedRealtime", "()J"),
            ("android/os/SystemClock", "elapsedRealtimeNanos", "()J"),
            ("android/os/SystemClock", "uptimeMillis", "()J"),
        ] {
            assert!(
                fake_jvm::registered_native(class, name, sig).is_some(),
                "{class}.{name}{sig} is bound before libroblox's JNI_OnLoad"
            );
        }
    }

    #[test]
    fn android_clocks_never_precede_the_process_start_time() {
        assert!(monotonic_millis() >= jlong::try_from(ATL_PROCESS_START_MILLIS).unwrap());
        assert!(
            monotonic_nanos() >= jlong::try_from(ATL_PROCESS_START_MILLIS * 1_000_000).unwrap()
        );
    }

    #[test]
    fn monotonic_anchor_clock_is_non_decreasing() {
        let anchor = MONOTONIC_ANCHOR.get_or_init(Instant::now);
        let first = anchor.elapsed().as_millis();
        let second = anchor.elapsed().as_millis();
        assert!(
            second >= first,
            "elapsedRealtime must be monotonic: {first} then {second}"
        );

        let first_nanos = monotonic_nanos();
        let second_nanos = monotonic_nanos();
        assert!(
            second_nanos >= first_nanos,
            "elapsedRealtimeNanos must be monotonic: {first_nanos} then {second_nanos}"
        );
    }

    #[test]
    fn view_native_names_sigs_and_class_match_view_java() {
        assert_eq!(VIEW_CLASS.to_str(), "android/view/View");
        assert_eq!(VIEW_NATIVE_CONSTRUCTOR_NAME.to_str(), "native_constructor");
        assert_eq!(
            VIEW_NATIVE_CONSTRUCTOR_SIG.to_str(),
            "(Landroid/content/Context;Landroid/util/AttributeSet;)J"
        );

        assert_eq!(VIEW_NATIVE_SET_PADDING_NAME.to_str(), "native_setPadding");
        assert_eq!(VIEW_NATIVE_SET_PADDING_SIG.to_str(), "(JIIII)V");

        assert_eq!(
            VIEW_NATIVE_SET_LAYOUT_PARAMS_NAME.to_str(),
            "native_setLayoutParams"
        );
        assert_eq!(VIEW_NATIVE_SET_LAYOUT_PARAMS_SIG.to_str(), "(JIIIFIIII)V");

        assert_eq!(
            VIEW_NATIVE_REQUEST_LAYOUT_NAME.to_str(),
            "native_requestLayout"
        );
        assert_eq!(VIEW_NATIVE_REQUEST_LAYOUT_SIG.to_str(), "(J)V");

        assert_eq!(
            VIEW_NATIVE_SET_BACKGROUND_DRAWABLE_NAME.to_str(),
            "native_setBackgroundDrawable"
        );
        assert_eq!(VIEW_NATIVE_SET_BACKGROUND_DRAWABLE_SIG.to_str(), "(JJ)V");

        assert_eq!(
            VIEW_NATIVE_SET_VISIBILITY_NAME.to_str(),
            "native_setVisibility"
        );
        assert_eq!(VIEW_NATIVE_SET_VISIBILITY_SIG.to_str(), "(JIFZ)V");

        assert_eq!(
            VIEW_SET_ON_CLICK_LISTENER_NAME.to_str(),
            "nativeSetOnClickListener"
        );
        assert_eq!(VIEW_SET_ON_CLICK_LISTENER_SIG.to_str(), "(J)V");

        assert_eq!(
            VIEW_SET_ON_TOUCH_LISTENER_NAME.to_str(),
            "nativeSetOnTouchListener"
        );
        assert_eq!(VIEW_SET_ON_TOUCH_LISTENER_SIG.to_str(), "(J)V");

        assert_eq!(
            VIEW_SET_ON_LONG_CLICK_LISTENER_NAME.to_str(),
            "nativeSetOnLongClickListener"
        );
        assert_eq!(VIEW_SET_ON_LONG_CLICK_LISTENER_SIG.to_str(), "(J)V");

        assert_eq!(
            VIEW_SET_BACKGROUND_COLOR_NAME.to_str(),
            "native_setBackgroundColor"
        );
        assert_eq!(VIEW_SET_BACKGROUND_COLOR_SIG.to_str(), "(JI)V");

        assert_eq!(
            VIEW_GET_WINDOW_VISIBLE_DISPLAY_FRAME_NAME.to_str(),
            "getWindowVisibleDisplayFrame"
        );
        assert_eq!(
            VIEW_GET_WINDOW_VISIBLE_DISPLAY_FRAME_SIG.to_str(),
            "(Landroid/graphics/Rect;)V"
        );
        assert_eq!(
            VIEW_NATIVE_IS_ATTACHED_TO_WINDOW_NAME.to_str(),
            "nativeIsAttachedToWindow"
        );
        assert_eq!(VIEW_NATIVE_IS_ATTACHED_TO_WINDOW_SIG.to_str(), "(J)Z");

        assert_eq!(
            VIEW_NATIVE_SET_FULLSCREEN_NAME.to_str(),
            "nativeSetFullscreen"
        );
        assert_eq!(VIEW_NATIVE_SET_FULLSCREEN_SIG.to_str(), "(JZ)V");

        assert_eq!(VIEW_NATIVE_GET_WINDOW_NAME.to_str(), "native_get_window");
        assert_eq!(
            VIEW_NATIVE_GET_WINDOW_SIG.to_str(),
            "(J)Landroid/view/Window;"
        );

        assert_eq!(VIEW_NATIVE_DESTRUCTOR_NAME.to_str(), "native_destructor");
        assert_eq!(VIEW_NATIVE_DESTRUCTOR_SIG.to_str(), "(J)V");

        assert_eq!(VIEW_NATIVE_LAYOUT_NAME.to_str(), "native_layout");
        assert_eq!(VIEW_NATIVE_LAYOUT_SIG.to_str(), "(JIIII)V");

        assert_eq!(VIEW_NATIVE_IS_FOCUSED_NAME.to_str(), "nativeIsFocused");
        assert_eq!(VIEW_NATIVE_IS_FOCUSED_SIG.to_str(), "(J)Z");

        assert_eq!(VIEW_NATIVE_INVALIDATE_NAME.to_str(), "nativeInvalidate");
        assert_eq!(VIEW_NATIVE_INVALIDATE_SIG.to_str(), "(J)V");

        assert_eq!(
            VIEW_NATIVE_KEEP_SCREEN_ON_NAME.to_str(),
            "native_keep_screen_on"
        );
        assert_eq!(VIEW_NATIVE_KEEP_SCREEN_ON_SIG.to_str(), "(JZ)V");

        assert_eq!(VIEW_NATIVE_ADD_CLASS_NAME.to_str(), "native_addClass");
        assert_eq!(VIEW_NATIVE_ADD_CLASS_SIG.to_str(), "(JLjava/lang/String;)V");
        assert_eq!(
            VIEW_NATIVE_REMOVE_CLASSES_NAME.to_str(),
            "native_removeClasses"
        );
        assert_eq!(
            VIEW_NATIVE_REMOVE_CLASSES_SIG.to_str(),
            "(J[Ljava/lang/String;)V"
        );

        assert_eq!(
            VIEW_NATIVE_DRAW_BACKGROUND_NAME.to_str(),
            "native_drawBackground"
        );
        assert_eq!(VIEW_NATIVE_DRAW_BACKGROUND_SIG.to_str(), "(JJ)V");
        assert_eq!(VIEW_NATIVE_DRAW_CONTENT_NAME.to_str(), "native_drawContent");
        assert_eq!(VIEW_NATIVE_DRAW_CONTENT_SIG.to_str(), "(JJ)V");

        assert_eq!(
            VIEW_NATIVE_QUEUE_ALLOCATE_NAME.to_str(),
            "native_queueAllocate"
        );
        assert_eq!(VIEW_NATIVE_QUEUE_ALLOCATE_SIG.to_str(), "(J)V");

        assert_eq!(VIEW_NATIVE_MEASURE_NAME.to_str(), "native_measure");
        assert_eq!(VIEW_NATIVE_MEASURE_SIG.to_str(), "(JII)V");

        assert_eq!(
            VIEW_SET_MEASURED_DIMENSION_NAME.to_str(),
            "setMeasuredDimension"
        );
        assert_eq!(VIEW_SET_MEASURED_DIMENSION_SIG.sig().to_str(), "(II)V");
        assert_eq!(
            VIEW_GET_SUGGESTED_MIN_WIDTH_NAME.to_str(),
            "getSuggestedMinimumWidth"
        );
        assert_eq!(
            VIEW_GET_SUGGESTED_MIN_HEIGHT_NAME.to_str(),
            "getSuggestedMinimumHeight"
        );
        assert_eq!(VIEW_GET_SUGGESTED_MIN_SIG.sig().to_str(), "()I");

        assert_eq!(VIEW_WIDGET_FIELD_NAME.to_str(), "widget");
        assert_eq!(VIEW_WIDGET_FIELD_SIG.to_str(), "J");

        assert_eq!(ARRAY_LIST_SIG.to_str(), "Ljava/util/ArrayList;");
        assert_eq!(RBX_SURFACE_VIEW_CLASS, "com.roblox.client.RBXSurfaceView");
        assert_eq!(WINDOW_FORMAT_RGBA_8888, 1);

        assert_eq!(TEXT_VIEW_CLASS.to_str(), "android/widget/TextView");

        assert_eq!(TEXT_VIEW_NATIVE_SET_TEXT_NAME.to_str(), "native_setText");
        assert_eq!(
            TEXT_VIEW_NATIVE_SET_TEXT_SIG.to_str(),
            "(Ljava/lang/String;)V"
        );

        assert_eq!(
            TEXT_VIEW_NATIVE_SET_TEXT_COLOR_NAME.to_str(),
            "native_setTextColor"
        );
        assert_eq!(TEXT_VIEW_NATIVE_SET_TEXT_COLOR_SIG.to_str(), "(I)V");

        assert_eq!(TEXT_VIEW_SET_TEXT_SIZE_NAME.to_str(), "setTextSize");
        assert_eq!(TEXT_VIEW_SET_TEXT_SIZE_SIG.to_str(), "(F)V");

        assert_eq!(
            TEXT_VIEW_NATIVE_SET_MARKUP_NAME.to_str(),
            "native_set_markup"
        );
        assert_eq!(TEXT_VIEW_NATIVE_SET_MARKUP_SIG.to_str(), "(I)V");

        assert_eq!(
            TEXT_VIEW_NATIVE_SET_COMPOUND_DRAWABLES_NAME.to_str(),
            "native_setCompoundDrawables"
        );
        assert_eq!(
            TEXT_VIEW_NATIVE_SET_COMPOUND_DRAWABLES_SIG.to_str(),
            "(JJJJJ)V"
        );
    }

    #[test]
    fn measure_default_size_serves_installed_get_default_size_semantics() {
        assert_eq!(measure_default_size(MEASURE_SPEC_EXACTLY | 240, 17), 240);

        let s = MEASURE_SPEC_AT_MOST | 800;
        assert!(
            s < 0,
            "an AT_MOST spec must be a negative jint (sign-bit mode encoding)"
        );

        const { assert!(MEASURE_SPEC_MODE_MASK < 0) };
        assert_eq!(measure_default_size(s, 17), 800);

        assert_eq!(measure_default_size(0, 0), 0);
        assert_eq!(measure_default_size(55, 17), 17);

        assert_eq!(measure_default_size(MEASURE_SPEC_MODE_MASK | 320, 17), 17);

        assert_eq!(
            measure_default_size(MEASURE_SPEC_AT_MOST | 0x3FFF_FFFF, 17),
            0x3FFF_FFFF
        );
        assert_eq!(
            measure_default_size(MEASURE_SPEC_EXACTLY | 0x3FFF_FFFF, 17),
            0x3FFF_FFFF
        );
    }

    #[test]
    fn view_tree_observer_native_name_sig_and_class_match_view_tree_observer_java() {
        assert_eq!(
            VIEW_TREE_OBSERVER_CLASS.to_str(),
            "android/view/ViewTreeObserver"
        );
        assert_eq!(
            VIEW_TREE_OBSERVER_SET_HAVE_LISTENERS_NAME.to_str(),
            "native_set_have_global_layout_listeners"
        );
        assert_eq!(VIEW_TREE_OBSERVER_SET_HAVE_LISTENERS_SIG.to_str(), "(Z)V");
    }

    #[test]
    fn global_layout_listener_registration_queues_dispatch_from_main_pump() {
        let source = include_str!("framework.rs");
        let obsolete = ["no-op (no host ", "layout signal)"].concat();

        assert!(!source.contains(&obsolete));
        assert!(source.contains("GLOBAL_LAYOUT_OBSERVERS"));
        assert!(source.contains("let layout_result = dispatch_pending_global_layout(env);"));
        assert!(source.contains("dispatchOnGlobalLayout"));
    }

    #[test]
    fn window_native_names_sigs_and_class_match_window_java() {
        assert_eq!(WINDOW_CLASS.to_str(), "android/view/Window");
        assert_eq!(WINDOW_SET_JOBJECT_NAME.to_str(), "set_jobject");
        assert_eq!(WINDOW_SET_JOBJECT_SIG.to_str(), "(JLandroid/view/Window;)V");
        assert_eq!(WINDOW_SET_TITLE_NAME.to_str(), "set_title");
        assert_eq!(WINDOW_SET_TITLE_SIG.to_str(), "(JLjava/lang/String;)V");
        assert_eq!(WINDOW_SET_LAYOUT_NAME.to_str(), "set_layout");
        assert_eq!(WINDOW_SET_LAYOUT_SIG.to_str(), "(JII)V");
        assert_eq!(
            WINDOW_SET_WIDGET_AS_ROOT_NAME.to_str(),
            "set_widget_as_root"
        );
        assert_eq!(WINDOW_SET_WIDGET_AS_ROOT_SIG.to_str(), "(JJ)V");
        assert_eq!(
            WINDOW_REMOVE_GTK_BACKGROUND_NAME.to_str(),
            "remove_gtk_background"
        );
        assert_eq!(WINDOW_REMOVE_GTK_BACKGROUND_SIG.to_str(), "(J)V");
        assert_eq!(
            WINDOW_SET_SCREEN_BRIGHTNESS_NAME.to_str(),
            "set_screen_brightness"
        );
        assert_eq!(WINDOW_SET_SCREEN_BRIGHTNESS_SIG.to_str(), "(F)V");
        assert_eq!(WINDOW_TAKE_INPUT_QUEUE_NAME.to_str(), "take_input_queue");
        assert_eq!(
            WINDOW_TAKE_INPUT_QUEUE_SIG.to_str(),
            "(JLandroid/view/InputQueue$Callback;Landroid/view/InputQueue;)V"
        );
    }

    #[test]
    fn paint_native_name_sig_and_class_match_art_reported() {
        assert_eq!(PAINT_CLASS.to_str(), "android/graphics/Paint");
        assert_eq!(PAINT_NATIVE_CREATE_NAME.to_str(), "native_create");
        assert_eq!(PAINT_NATIVE_CREATE_SIG.to_str(), "()J");

        assert_eq!(PAINT_NATIVE_SET_COLOR_NAME.to_str(), "native_set_color");
        assert_eq!(PAINT_NATIVE_SET_COLOR_SIG.to_str(), "(JI)V");

        assert_eq!(
            PAINT_NATIVE_SET_STROKE_WIDTH_NAME.to_str(),
            "native_set_stroke_width"
        );
        assert_eq!(PAINT_NATIVE_SET_STROKE_WIDTH_SIG.to_str(), "(JF)V");

        assert_eq!(PAINT_NATIVE_SET_STYLE_NAME.to_str(), "native_set_style");
        assert_eq!(PAINT_NATIVE_SET_STYLE_SIG.to_str(), "(JI)V");

        assert_eq!(
            PAINT_NATIVE_SET_TEXT_SIZE_NAME.to_str(),
            "native_set_text_size"
        );
        assert_eq!(PAINT_NATIVE_SET_TEXT_SIZE_SIG.to_str(), "(JF)V");

        assert_eq!(PAINT_NATIVE_CLONE_NAME.to_str(), "native_clone");
        assert_eq!(PAINT_NATIVE_CLONE_SIG.to_str(), "(J)J");
        assert_eq!(PAINT_NATIVE_RECYCLE_NAME.to_str(), "native_recycle");
        assert_eq!(PAINT_NATIVE_RECYCLE_SIG.to_str(), "(J)V");
        assert_eq!(PAINT_NATIVE_GET_COLOR_NAME.to_str(), "native_get_color");
        assert_eq!(PAINT_NATIVE_GET_COLOR_SIG.to_str(), "(J)I");
        assert_eq!(PAINT_NATIVE_SET_ALPHA_NAME.to_str(), "native_set_alpha");
        assert_eq!(PAINT_NATIVE_SET_ALPHA_SIG.to_str(), "(JI)V");
        assert_eq!(PAINT_NATIVE_GET_ALPHA_NAME.to_str(), "native_get_alpha");
        assert_eq!(PAINT_NATIVE_GET_ALPHA_SIG.to_str(), "(J)I");
        assert_eq!(PAINT_NATIVE_GET_STYLE_NAME.to_str(), "native_get_style");
        assert_eq!(PAINT_NATIVE_GET_STYLE_SIG.to_str(), "(J)I");
        assert_eq!(
            PAINT_NATIVE_GET_STROKE_WIDTH_NAME.to_str(),
            "native_get_stroke_width"
        );
        assert_eq!(PAINT_NATIVE_GET_STROKE_WIDTH_SIG.to_str(), "(J)F");
        assert_eq!(
            PAINT_NATIVE_SET_STROKE_CAP_NAME.to_str(),
            "native_set_stroke_cap"
        );
        assert_eq!(PAINT_NATIVE_SET_STROKE_CAP_SIG.to_str(), "(JI)V");
        assert_eq!(
            PAINT_NATIVE_GET_STROKE_CAP_NAME.to_str(),
            "native_get_stroke_cap"
        );
        assert_eq!(PAINT_NATIVE_GET_STROKE_CAP_SIG.to_str(), "(J)I");
        assert_eq!(
            PAINT_NATIVE_SET_STROKE_JOIN_NAME.to_str(),
            "native_set_stroke_join"
        );
        assert_eq!(PAINT_NATIVE_SET_STROKE_JOIN_SIG.to_str(), "(JI)V");
        assert_eq!(
            PAINT_NATIVE_GET_STROKE_JOIN_NAME.to_str(),
            "native_get_stroke_join"
        );
        assert_eq!(PAINT_NATIVE_GET_STROKE_JOIN_SIG.to_str(), "(J)I");
        assert_eq!(
            PAINT_NATIVE_GET_TEXT_SIZE_NAME.to_str(),
            "native_get_text_size"
        );
        assert_eq!(PAINT_NATIVE_GET_TEXT_SIZE_SIG.to_str(), "(J)F");
        assert_eq!(
            PAINT_NATIVE_SET_COLOR_FILTER_NAME.to_str(),
            "native_set_color_filter"
        );
        assert_eq!(PAINT_NATIVE_SET_COLOR_FILTER_SIG.to_str(), "(JII)V");
        assert_eq!(
            PAINT_NATIVE_SET_TEXT_ALIGN_NAME.to_str(),
            "native_set_text_align"
        );
        assert_eq!(PAINT_NATIVE_SET_TEXT_ALIGN_SIG.to_str(), "(JI)V");
    }

    #[test]
    fn paint_color_with_alpha_replaces_only_the_alpha_channel() {
        assert_eq!(
            paint_color_with_alpha(0x1234_5678, 0xFF),
            0xFF34_5678u32 as i32
        );
        assert_eq!(
            paint_color_with_alpha(0xFF34_5678u32 as i32, 0),
            0x0034_5678
        );
        assert_eq!(
            paint_color_with_alpha(0x8000_0001u32 as i32, 0x80),
            0x8000_0001u32 as i32
        );

        assert_eq!(
            paint_color_with_alpha(0x0034_5678, 0x1FF),
            0xFF34_5678u32 as i32
        );

        let merged = paint_color_with_alpha(0x0012_3456, 0xAB);
        assert_eq!((merged >> 24) & 0xFF, 0xAB);
    }

    #[test]
    fn matrix_native_name_sig_and_class_match_art_reported() {
        assert_eq!(MATRIX_CLASS.to_str(), "android/graphics/Matrix");
        assert_eq!(MATRIX_NATIVE_CREATE_NAME.to_str(), "native_create");
        assert_eq!(MATRIX_NATIVE_CREATE_SIG.to_str(), "(J)J");

        assert_eq!(MATRIX_FINALIZER_NAME.to_str(), "finalizer");
        assert_eq!(MATRIX_FINALIZER_SIG.to_str(), "(J)V");
    }

    #[test]
    fn path_native_names_sigs_and_class_match_art_reported() {
        assert_eq!(PATH_CLASS.to_str(), "android/graphics/Path");
        assert_eq!(
            PATH_NATIVE_CREATE_BUILDER_NAME.to_str(),
            "native_create_builder"
        );
        assert_eq!(PATH_NATIVE_CREATE_BUILDER_SIG.to_str(), "(JJ)J");
        assert_eq!(PATH_NATIVE_MOVE_TO_NAME.to_str(), "native_move_to");
        assert_eq!(PATH_NATIVE_MOVE_TO_SIG.to_str(), "(JFF)V");
        assert_eq!(PATH_NATIVE_LINE_TO_NAME.to_str(), "native_line_to");
        assert_eq!(PATH_NATIVE_LINE_TO_SIG.to_str(), "(JFF)V");
        assert_eq!(PATH_NATIVE_QUAD_TO_NAME.to_str(), "native_quad_to");
        assert_eq!(PATH_NATIVE_QUAD_TO_SIG.to_str(), "(JFFFF)V");
        assert_eq!(PATH_NATIVE_CUBIC_TO_NAME.to_str(), "native_cubic_to");
        assert_eq!(PATH_NATIVE_CUBIC_TO_SIG.to_str(), "(JFFFFFF)V");
        assert_eq!(PATH_NATIVE_CLOSE_NAME.to_str(), "native_close");
        assert_eq!(PATH_NATIVE_CLOSE_SIG.to_str(), "(J)V");
        assert_eq!(PATH_NATIVE_CREATE_PATH_NAME.to_str(), "native_create_path");
        assert_eq!(PATH_NATIVE_CREATE_PATH_SIG.to_str(), "(J)J");
        assert_eq!(PATH_NATIVE_REF_PATH_NAME.to_str(), "native_ref_path");
        assert_eq!(PATH_NATIVE_REF_PATH_SIG.to_str(), "(J)J");

        assert_eq!(PATH_NATIVE_RESET_NAME.to_str(), "native_reset");
        assert_eq!(PATH_NATIVE_RESET_SIG.to_str(), "(JJ)V");
    }

    #[test]
    fn canvas_native_names_and_sigs() {
        assert_eq!(CANVAS_CLASS.to_str(), "android/graphics/Canvas");
        assert_eq!(CANVAS_N_DRAW_COLOR_NAME.to_str(), "nDrawColor");
        assert_eq!(CANVAS_N_DRAW_COLOR_SIG.sig().to_str(), "(JI)V");
        assert_eq!(CANVAS_N_DRAW_RECT_NAME.to_str(), "nDrawRect");
        assert_eq!(CANVAS_N_DRAW_RECT_SIG.sig().to_str(), "(JFFFFJ)V");
        assert_eq!(CANVAS_N_DRAW_CIRCLE_NAME.to_str(), "nDrawCircle");
        assert_eq!(CANVAS_N_DRAW_CIRCLE_SIG.sig().to_str(), "(JFFFJ)V");
        assert_eq!(CANVAS_N_DRAW_PATH_NAME.to_str(), "nDrawPath");
        assert_eq!(CANVAS_N_DRAW_PATH_SIG.sig().to_str(), "(JJJ)V");
    }

    #[test]
    fn paint_config_from_handle_reads_paint_then_defaults_when_invalid() {
        let p = paint_registry::allocate().expect("allocate paint");
        paint_registry::with_paint(p, |s| {
            s.color = 0x80AB_CDEFu32 as i32;
            s.style = paint_registry::PaintStyle::Stroke;
            s.stroke_width = 3.5;
        })
        .expect("configure paint");
        let cfg = paint_config_from_handle(p);
        assert_eq!(cfg.argb, 0x80AB_CDEFu32 as i32);
        assert_eq!(cfg.style, paint_registry::PaintStyle::Stroke);
        assert_eq!(cfg.stroke_width, 3.5);
        paint_registry::free(p).expect("free paint");

        let def = paint_config_from_handle(0);
        assert_eq!(def.argb, canvas_registry::PaintConfig::default().argb);
        assert_eq!(def.style, paint_registry::PaintStyle::Fill);
    }

    #[test]
    fn draw_target_and_drawn_canvas_are_plain_copy_values() {
        let t = DrawTarget {
            handle: 42,
            width: 100,
            height: 50,
        };
        let t2 = t;
        assert_eq!(t, t2);
        assert_eq!(t.width, 100);
        assert_eq!(t.height, 50);
        let d = DrawnCanvas {
            view: 42,
            canvas: 7,
        };
        assert_eq!(d.view, t.handle);
        assert_eq!(d.canvas, 7);
    }

    #[test]
    fn image_view_class_is_slashed_internal_name() {
        assert_eq!(IMAGE_VIEW_CLASS.to_str(), "android/widget/ImageView");

        assert_eq!(
            IMAGE_VIEW_SET_SCALE_TYPE_NAME.to_str(),
            "native_setScaleType"
        );
        assert_eq!(IMAGE_VIEW_SET_SCALE_TYPE_SIG.to_str(), "(JI)V");

        assert_eq!(IMAGE_VIEW_SET_DRAWABLE_NAME.to_str(), "native_setDrawable");
        assert_eq!(IMAGE_VIEW_SET_DRAWABLE_SIG.to_str(), "(JJ)V");
    }

    #[test]
    fn image_button_class_is_slashed_internal_name() {
        assert_eq!(IMAGE_BUTTON_CLASS.to_str(), "android/widget/ImageButton");

        assert_eq!(
            IMAGE_BUTTON_SET_ON_CLICK_LISTENER_NAME.to_str(),
            "nativeSetOnClickListener"
        );
        assert_eq!(IMAGE_BUTTON_SET_ON_CLICK_LISTENER_SIG.to_str(), "(J)V");

        assert_eq!(IMAGE_VIEW_SET_DRAWABLE_NAME.to_str(), "native_setDrawable");
        assert_eq!(IMAGE_VIEW_SET_DRAWABLE_SIG.to_str(), "(JJ)V");
    }

    #[test]
    fn surface_view_class_is_slashed_internal_name() {
        assert_eq!(SURFACE_VIEW_CLASS.to_str(), "android/view/SurfaceView");
    }

    #[test]
    fn view_subclass_constructor_classes_are_slashed_internal_names() {
        let names: Vec<String> = VIEW_SUBCLASS_CONSTRUCTOR_CLASSES
            .iter()
            .map(|c| c.to_str().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "android/widget/Button",
                "android/widget/EditText",
                "android/widget/ProgressBar",
                "android/widget/CheckBox",
                "android/widget/RadioButton",
                "android/widget/SeekBar",
                "android/widget/Spinner",
                "android/widget/ScrollView",
            ],
        );

        assert!(!names.iter().any(|n| n == "android/widget/CompoundButton"));
        assert!(!names.iter().any(|n| n == "android/widget/PopupWindow"));

        assert!(!names.iter().any(|n| n == "android/webkit/WebView"));
    }

    #[test]
    fn activity_manager_memory_native_names_sigs_and_class_match_the_overlay() {
        assert_eq!(
            ACTIVITY_MANAGER_CLASS.to_str(),
            "android/app/ActivityManager"
        );
        assert_eq!(
            AM_NATIVE_FILL_MEMORY_INFO_NAME.to_str(),
            "native_fillMemoryInfo"
        );
        assert_eq!(
            AM_NATIVE_FILL_MEMORY_INFO_SIG.to_str(),
            "(Landroid/app/ActivityManager$MemoryInfo;)V"
        );
        assert_eq!(
            AM_NATIVE_GET_MEMORY_CLASS_NAME.to_str(),
            "native_getMemoryClass"
        );
        assert_eq!(AM_NATIVE_GET_MEMORY_CLASS_SIG.to_str(), "()I");
        assert_eq!(
            AM_NATIVE_GET_LARGE_MEMORY_CLASS_NAME.to_str(),
            "native_getLargeMemoryClass"
        );
        assert_eq!(AM_NATIVE_GET_LARGE_MEMORY_CLASS_SIG.to_str(), "()I");
        assert_eq!(
            AM_NATIVE_IS_LOW_RAM_DEVICE_NAME.to_str(),
            "native_isLowRamDevice"
        );
        assert_eq!(AM_NATIVE_IS_LOW_RAM_DEVICE_SIG.to_str(), "()Z");
        assert_eq!(memory_bytes_to_jlong(u64::MAX), jlong::MAX);
    }

    #[test]
    fn web_view_native_names_sigs_and_class_match_the_installed_dex() {
        assert_eq!(WEB_VIEW_CLASS.to_str(), "android/webkit/WebView");

        assert_eq!(VIEW_NATIVE_CONSTRUCTOR_NAME.to_str(), "native_constructor");
        assert_eq!(
            VIEW_NATIVE_CONSTRUCTOR_SIG.to_str(),
            "(Landroid/content/Context;Landroid/util/AttributeSet;)J"
        );

        assert_eq!(WEB_VIEW_NATIVE_LOAD_URL_NAME.to_str(), "native_loadUrl");
        assert_eq!(
            WEB_VIEW_NATIVE_LOAD_URL_SIG.to_str(),
            "(JLjava/lang/String;)V"
        );
        assert_eq!(
            WEB_VIEW_NATIVE_LOAD_DATA_WITH_BASE_URL_NAME.to_str(),
            "native_loadDataWithBaseURL"
        );
        assert_eq!(
            WEB_VIEW_NATIVE_LOAD_DATA_WITH_BASE_URL_SIG.to_str(),
            "(JLjava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V"
        );

        assert_eq!(
            WEB_VIEW_NATIVE_EVALUATE_JAVASCRIPT_NAME.to_str(),
            "native_evaluateJavascript"
        );
        assert_eq!(
            WEB_VIEW_NATIVE_EVALUATE_JAVASCRIPT_SIG.to_str(),
            "(JLjava/lang/String;Landroid/webkit/ValueCallback;)V"
        );
        assert_eq!(
            WEB_VIEW_NATIVE_ADD_JAVASCRIPT_INTERFACE_NAME.to_str(),
            "native_addJavascriptInterface"
        );
        assert_eq!(
            WEB_VIEW_NATIVE_ADD_JAVASCRIPT_INTERFACE_SIG.to_str(),
            "(JLjava/lang/Object;Ljava/lang/String;)V"
        );
        assert_eq!(
            WEB_VIEW_NATIVE_CAN_GO_BACK_NAME.to_str(),
            "native_canGoBack"
        );
        assert_eq!(WEB_VIEW_NATIVE_CAN_GO_BACK_SIG.to_str(), "(J)Z");
        assert_eq!(WEB_VIEW_NATIVE_GO_BACK_NAME.to_str(), "native_goBack");
        assert_eq!(WEB_VIEW_NATIVE_GO_BACK_SIG.to_str(), "(J)V");
        assert_eq!(WEB_VIEW_NATIVE_RELOAD_NAME.to_str(), "native_reload");
        assert_eq!(WEB_VIEW_NATIVE_RELOAD_SIG.to_str(), "(J)V");
        assert_eq!(
            WEB_VIEW_NATIVE_STOP_LOADING_NAME.to_str(),
            "native_stopLoading"
        );
        assert_eq!(WEB_VIEW_NATIVE_STOP_LOADING_SIG.to_str(), "(J)V");
        assert_eq!(WEB_VIEW_NATIVE_GET_URL_NAME.to_str(), "native_getUrl");
        assert_eq!(
            WEB_VIEW_NATIVE_GET_URL_SIG.to_str(),
            "(J)Ljava/lang/String;"
        );
        assert_eq!(
            WEB_VIEW_NATIVE_REMOVE_JAVASCRIPT_INTERFACE_NAME.to_str(),
            "native_removeJavascriptInterface"
        );
        assert_eq!(
            WEB_VIEW_NATIVE_REMOVE_JAVASCRIPT_INTERFACE_SIG.to_str(),
            "(JLjava/lang/String;)V"
        );
        assert_eq!(WEB_VIEW_NATIVE_DESTROY_NAME.to_str(), "native_destroy");
        assert_eq!(WEB_VIEW_NATIVE_DESTROY_SIG.to_str(), "(J)V");

        assert_eq!(WEB_SETTINGS_CLASS.to_str(), "android/webkit/WebSettings");
        assert_eq!(
            WEB_SETTINGS_NATIVE_SET_USER_AGENT_STRING_NAME.to_str(),
            "native_setUserAgentString"
        );
        assert_eq!(
            WEB_SETTINGS_NATIVE_SET_USER_AGENT_STRING_SIG.to_str(),
            "(JLjava/lang/String;)V"
        );
        assert_eq!(
            WEB_RESOURCE_REQUEST_CLASS.to_str(),
            "android/webkit/EclipseWebResourceRequest"
        );
        assert_eq!(
            WEB_RESOURCE_ERROR_CLASS.to_str(),
            "android/webkit/WebResourceError"
        );

        assert_eq!(
            COOKIE_MANAGER_CLASS.to_str(),
            "android/webkit/CookieManager"
        );
        assert_eq!(CM_NATIVE_GET_COOKIE_NAME.to_str(), "native_getCookie");
        assert_eq!(
            CM_NATIVE_GET_COOKIE_SIG.to_str(),
            "(Ljava/lang/String;)Ljava/lang/String;"
        );
        assert_eq!(CM_NATIVE_SET_COOKIE_NAME.to_str(), "native_setCookie");
        assert_eq!(
            CM_NATIVE_SET_COOKIE_SIG.to_str(),
            "(Ljava/lang/String;Ljava/lang/String;)V"
        );
        assert_eq!(
            CM_NATIVE_SET_COOKIE_CB_SIG.to_str(),
            "(Ljava/lang/String;Ljava/lang/String;Landroid/webkit/ValueCallback;)V"
        );
        assert_eq!(
            CM_NATIVE_REMOVE_ALL_COOKIES_NAME.to_str(),
            "native_removeAllCookies"
        );
        assert_eq!(
            CM_NATIVE_REMOVE_SESSION_COOKIES_NAME.to_str(),
            "native_removeSessionCookies"
        );
        assert_eq!(CM_NATIVE_FLUSH_NAME.to_str(), "native_flush");
        assert_eq!(CM_NATIVE_FLUSH_SIG.to_str(), "()V");
        assert_eq!(
            JAVASCRIPT_INTERFACE_CLASS.to_str(),
            "android/webkit/JavascriptInterface"
        );
    }

    #[test]
    fn cookie_manager_url_fixup_matches_androids_relaxed_host_boundary() {
        assert_eq!(
            fixup_webview_cookie_url("roblox.com"),
            CookieUrlFixup {
                url: "http://roblox.com/".to_string(),
                implied_domain: None,
            }
        );
        assert_eq!(
            fixup_webview_cookie_url("roblox.com/account"),
            CookieUrlFixup {
                url: "http://roblox.com/account".to_string(),
                implied_domain: None,
            }
        );
        assert_eq!(
            fixup_webview_cookie_url("roblox.com:443"),
            CookieUrlFixup {
                url: "https://roblox.com:443/".to_string(),
                implied_domain: None,
            }
        );
        assert_eq!(
            fixup_webview_cookie_url(".roblox.com"),
            CookieUrlFixup {
                url: "http://roblox.com/".to_string(),
                implied_domain: Some(".roblox.com".to_string()),
            }
        );
        let full = "https://auth.roblox.com/v2/login";
        assert_eq!(fixup_webview_cookie_url(full).url, full);
        let invalid = "not a host value";
        assert_eq!(fixup_webview_cookie_url(invalid).url, invalid);
    }

    #[test]
    fn a_set_cookie_for_a_dotted_host_gains_that_domain_unless_it_names_one() {
        assert_eq!(
            set_cookie_request(".roblox.com", "session=value; Path=/".to_string()),
            (
                "http://roblox.com/".to_string(),
                "session=value; Path=/; Domain=.roblox.com".to_string()
            )
        );
        assert_eq!(
            set_cookie_request(
                ".roblox.com",
                "session=value; domain=auth.roblox.com".to_string()
            )
            .1,
            "session=value; domain=auth.roblox.com"
        );
        assert_eq!(
            set_cookie_request(
                "https://www.roblox.com",
                ".ROBLOSECURITY=v; Path=/; Secure; HttpOnly".to_string()
            ),
            (
                "https://www.roblox.com".to_string(),
                ".ROBLOSECURITY=v; Path=/; Secure; HttpOnly".to_string()
            ),
            "a full URL and a header pass through unchanged"
        );
        assert!(
            !set_cookie_has_attribute("domain=x; Path=/", "domain"),
            "the cookie's own name is not an attribute"
        );
        assert!(set_cookie_has_attribute("a=b; Secure", "secure"));
    }

    #[test]
    fn format_cookies_joins_name_value_with_semicolons() {
        use crate::webview::proto::CookiePair;
        let entry = |n: &str, v: &str| CookiePair {
            name: n.to_string(),
            value: v.to_string(),
        };
        assert_eq!(format_cookies(&[]), "");
        assert_eq!(format_cookies(&[entry("a", "1")]), "a=1");
        assert_eq!(
            format_cookies(&[entry("a", "1"), entry("b", "2")]),
            "a=1; b=2"
        );
    }

    #[test]
    fn bridge_args_marshal_supported_types_and_reject_unsupported() {
        use serde_json::json;
        assert_eq!(plan_arg(&json!("hi"), "java.lang.String"), ArgKind::Str);
        assert_eq!(
            plan_arg(&json!("hi"), "java.lang.CharSequence"),
            ArgKind::Str
        );
        assert_eq!(plan_arg(&json!("hi"), "java.lang.Object"), ArgKind::Str);
        assert_eq!(plan_arg(&json!("hi"), "int"), ArgKind::Reject);
        assert_eq!(plan_arg(&json!(3), "int"), ArgKind::IntBox);
        assert_eq!(plan_arg(&json!(3), "long"), ArgKind::LongBox);
        assert_eq!(plan_arg(&json!(3), "double"), ArgKind::DoubleBox);
        assert_eq!(plan_arg(&json!(3), "java.lang.Object"), ArgKind::DoubleBox);
        assert_eq!(plan_arg(&json!(3), "java.lang.String"), ArgKind::Reject);
        assert_eq!(plan_arg(&json!(true), "boolean"), ArgKind::BoolBox);
        assert_eq!(plan_arg(&json!(true), "int"), ArgKind::Reject);
        assert_eq!(plan_arg(&json!(null), "java.lang.String"), ArgKind::Null);

        assert_eq!(plan_arg(&json!(null), "int"), ArgKind::Reject);

        assert_eq!(
            plan_arg(&json!([1, 2]), "java.lang.Object"),
            ArgKind::Reject
        );
        assert_eq!(
            plan_arg(&json!({"a":1}), "java.lang.Object"),
            ArgKind::Reject
        );
    }

    #[test]
    fn bridge_arg_lens_returns_serialized_lengths_and_never_the_values() {
        use serde_json::json;
        let args = vec![json!("SECRETTOKEN"), json!(42), json!(true), json!(null)];
        let lens = bridge_arg_lens(&args);

        assert_eq!(lens, vec![13, 2, 4, 4]);
        assert!(
            !format!("{lens:?}").contains("SECRET"),
            "arg_lens must never echo an arg value"
        );
        assert_eq!(bridge_arg_lens(&[]), Vec::<usize>::new());
    }

    #[test]
    fn bridge_identifier_for_log_passes_identifiers_and_redacts_page_controlled_shapes() {
        assert_eq!(
            bridge_identifier_for_log("__globalRobloxAndroidBridge__"),
            "__globalRobloxAndroidBridge__"
        );
        assert_eq!(bridge_identifier_for_log("emitEvent"), "emitEvent");
        assert_eq!(bridge_identifier_for_log("$fn_1"), "$fn_1");

        for hostile in [
            "https://apps.roblox.com/challenge?token=SECRETTOKEN",
            "name with spaces",
            "1leadingdigit",
            "",
            "semi;colon",
        ] {
            assert_eq!(bridge_identifier_for_log(hostile), "<non-identifier>");
        }

        let over = "A".repeat(65);
        assert_eq!(bridge_identifier_for_log(&over), "<non-identifier>");
        let max = "A".repeat(64);
        assert_eq!(bridge_identifier_for_log(&max), max);
    }

    #[test]
    fn bridge_return_number_or_quote_embeds_valid_numbers_and_quotes_the_rest() {
        assert_eq!(number_or_quote("42"), "42");
        assert_eq!(number_or_quote("-1.5"), "-1.5");
        assert_eq!(number_or_quote("NaN"), "\"NaN\"");
        assert_eq!(number_or_quote("Infinity"), "\"Infinity\"");
    }

    #[test]
    fn bridge_overloads_resolve_by_arity_like_the_android_java_bridge() {
        assert_eq!(select_overload_index(&[1, 2], 2), Some(1));
        assert_eq!(select_overload_index(&[1, 2], 1), Some(0));

        assert_eq!(select_overload_index(&[1, 2], 3), None);
        assert_eq!(select_overload_index(&[], 0), None);

        assert_eq!(select_overload_index(&[2, 2], 2), Some(0));
    }

    #[test]
    fn eval_drain_victims_selects_only_pre_close_era_callbacks_of_the_closed_view() {
        let mut m = std::collections::HashMap::new();
        m.insert(1_u32, (7_i64, 0_u64, ()));
        m.insert(2_u32, (7_i64, 3_u64, ()));
        m.insert(3_u32, (7_i64, 4_u64, ()));
        m.insert(4_u32, (9_i64, 0_u64, ()));
        let mut victims = eval_drain_victims(&m, 7, 3);
        victims.sort_unstable();
        assert_eq!(victims, vec![1, 2]);

        assert_eq!(eval_drain_victims(&m, 9, 0), vec![4]);
    }

    #[test]
    fn webview_callback_gate_prefers_the_main_looper_and_degrades_honestly() {
        use MainDispatchGate::*;
        assert_eq!(main_dispatch_gate(false, true, true, true), Post);
        assert_eq!(
            main_dispatch_gate(true, true, true, true),
            InlineOnMainThread
        );

        assert_eq!(
            main_dispatch_gate(true, false, false, false),
            InlineOnMainThread
        );
        assert_eq!(
            main_dispatch_gate(false, false, true, true),
            InlineNoMainLooper
        );
        assert_eq!(
            main_dispatch_gate(false, true, false, true),
            InlineDrainRetired
        );
        assert_eq!(main_dispatch_gate(false, true, true, false), InlineSlotBusy);
    }

    #[test]
    fn bridge_survives_view_close_only_for_entries_born_after_the_close() {
        assert!(!bridge_survives_view_close(7, 3, 7, 3));
        assert!(!bridge_survives_view_close(7, 0, 7, 3));
        assert!(bridge_survives_view_close(7, 4, 7, 3));
        assert!(bridge_survives_view_close(9, 0, 7, 3));
    }

    #[test]
    fn widget_property_setter_names_sigs_and_classes_match_overlay() {
        assert_eq!(BUTTON_CLASS.to_str(), "android/widget/Button");
        assert_eq!(EDIT_TEXT_CLASS.to_str(), "android/widget/EditText");
        assert_eq!(CHECK_BOX_CLASS.to_str(), "android/widget/CheckBox");
        assert_eq!(RADIO_BUTTON_CLASS.to_str(), "android/widget/RadioButton");
        assert_eq!(PROGRESS_BAR_CLASS.to_str(), "android/widget/ProgressBar");
        assert_eq!(SEEK_BAR_CLASS.to_str(), "android/widget/SeekBar");
        assert_eq!(SPINNER_CLASS.to_str(), "android/widget/Spinner");
        assert_eq!(SCROLL_VIEW_CLASS.to_str(), "android/widget/ScrollView");

        assert_eq!(WIDGET_NATIVE_SET_TEXT_NAME.to_str(), "native_setText");
        assert_eq!(
            WIDGET_NATIVE_SET_TEXT_SIG.to_str(),
            "(JLjava/lang/String;)V"
        );

        assert_eq!(RADIO_BUTTON_SET_TEXT_NAME.to_str(), "setText");
        assert_eq!(
            RADIO_BUTTON_SET_TEXT_SIG.to_str(),
            "(Ljava/lang/CharSequence;)V"
        );

        assert_eq!(
            PROGRESS_BAR_SET_INDETERMINATE_NAME.to_str(),
            "native_setIndeterminate"
        );
        assert_eq!(PROGRESS_BAR_SET_INDETERMINATE_SIG.to_str(), "(Z)V");

        assert_eq!(
            PROGRESS_NATIVE_SET_PROGRESS_NAME.to_str(),
            "native_setProgress"
        );
        assert_eq!(PROGRESS_NATIVE_SET_PROGRESS_SIG.to_str(), "(JF)V");

        assert_eq!(SEEK_BAR_SET_MAX_NAME.to_str(), "native_setMax");
        assert_eq!(SEEK_BAR_SET_MAX_SIG.to_str(), "(JI)V");

        assert_eq!(
            BUTTON_SET_COMPOUND_DRAWABLES_NAME.to_str(),
            "native_setCompoundDrawables"
        );
        assert_eq!(BUTTON_SET_COMPOUND_DRAWABLES_SIG.to_str(), "(JJ)V");

        assert_eq!(SPINNER_SET_ADAPTER_NAME.to_str(), "native_setAdapter");
        assert_eq!(
            SPINNER_SET_ADAPTER_SIG.to_str(),
            "(JLandroid/widget/SpinnerAdapter;)V"
        );

        assert_eq!(VIEW_GROUP_NATIVE_ADD_VIEW_NAME.to_str(), "native_addView");
        assert_eq!(
            VIEW_GROUP_NATIVE_ADD_VIEW_SIG.to_str(),
            "(JJILandroid/view/ViewGroup$LayoutParams;)V"
        );
        assert_eq!(
            VIEW_GROUP_NATIVE_REMOVE_VIEW_NAME.to_str(),
            "native_removeView"
        );
        assert_eq!(VIEW_GROUP_NATIVE_REMOVE_VIEW_SIG.to_str(), "(JJ)V");

        assert_eq!(
            EDIT_TEXT_ADD_TEXT_CHANGED_LISTENER_NAME.to_str(),
            "native_addTextChangedListener"
        );
        assert_eq!(
            EDIT_TEXT_REMOVE_TEXT_CHANGED_LISTENER_NAME.to_str(),
            "native_removeTextChangedListener"
        );
        assert_eq!(
            EDIT_TEXT_TEXT_CHANGED_LISTENER_SIG.to_str(),
            "(JLandroid/text/TextWatcher;)V"
        );
        assert_eq!(
            EDIT_TEXT_SET_ON_EDITOR_ACTION_LISTENER_NAME.to_str(),
            "native_setOnEditorActionListener"
        );
        assert_eq!(
            EDIT_TEXT_SET_ON_EDITOR_ACTION_LISTENER_SIG.to_str(),
            "(JLandroid/widget/TextView$OnEditorActionListener;)V"
        );
    }

    #[test]
    fn register_class_natives_best_effort_skips_unbindable_method_and_continues() {
        let bindings: [NativeBinding; 3] = [
            (
                VIEW_NATIVE_CONSTRUCTOR_NAME,
                VIEW_NATIVE_CONSTRUCTOR_SIG,
                std::ptr::null_mut(),
            ),
            (
                VIEW_SET_BACKGROUND_COLOR_NAME,
                VIEW_SET_BACKGROUND_COLOR_SIG,
                std::ptr::null_mut(),
            ),
            (
                VIEW_NATIVE_DESTRUCTOR_NAME,
                VIEW_NATIVE_DESTRUCTOR_SIG,
                std::ptr::null_mut(),
            ),
        ];

        let mut visited: Vec<String> = Vec::new();
        let bound = fold_best_effort(&bindings, |&(name, _sig, _ptr)| {
            visited.push(name.to_str().into_owned());

            name.to_str() != VIEW_SET_BACKGROUND_COLOR_NAME.to_str()
        });

        assert_eq!(
            visited,
            vec![
                VIEW_NATIVE_CONSTRUCTOR_NAME.to_str().into_owned(),
                VIEW_SET_BACKGROUND_COLOR_NAME.to_str().into_owned(),
                VIEW_NATIVE_DESTRUCTOR_NAME.to_str().into_owned(),
            ],
            "a single unbindable entry must not short-circuit the remaining methods"
        );

        assert_eq!(
            bound, 2,
            "skipped entry is not counted; the rest still bind"
        );
    }

    #[test]
    fn drawable_native_name_sig_and_class_match_art_reported() {
        assert_eq!(
            DRAWABLE_CLASS.to_str(),
            "android/graphics/drawable/Drawable"
        );
        assert_eq!(
            DRAWABLE_NATIVE_CONSTRUCTOR_NAME.to_str(),
            "native_constructor"
        );
        assert_eq!(DRAWABLE_NATIVE_CONSTRUCTOR_SIG.to_str(), "()J");

        assert_eq!(DRAWABLE_NATIVE_UNREF_NAME.to_str(), "native_unref");
        assert_eq!(DRAWABLE_NATIVE_UNREF_SIG.to_str(), "(J)V");

        assert_eq!(
            DRAWABLE_NATIVE_INVALIDATE_NAME.to_str(),
            "native_invalidate"
        );
        assert_eq!(DRAWABLE_NATIVE_INVALIDATE_SIG.to_str(), "(J)V");

        assert_eq!(DRAWABLE_NATIVE_REF_NAME.to_str(), "native_ref");
        assert_eq!(DRAWABLE_NATIVE_REF_SIG.to_str(), "(J)V");
        assert_eq!(DRAWABLE_NATIVE_DRAW_NAME.to_str(), "native_draw");
        assert_eq!(DRAWABLE_NATIVE_DRAW_SIG.to_str(), "(JJII)V");
        assert_eq!(
            DRAWABLE_PAINTABLE_FROM_PATH_NAME.to_str(),
            "native_paintable_from_path"
        );
        assert_eq!(
            DRAWABLE_PAINTABLE_FROM_PATH_SIG.to_str(),
            "(Ljava/lang/String;)J"
        );

        assert_eq!(
            DRAWABLE_CONTAINER_CLASS.to_str(),
            "android/graphics/drawable/DrawableContainer"
        );
        assert_eq!(
            DRAWABLE_CONTAINER_SELECT_CHILD_NAME.to_str(),
            "native_selectChild"
        );
        assert_eq!(DRAWABLE_CONTAINER_SELECT_CHILD_SIG.to_str(), "(JJ)V");

        assert_eq!(
            NINE_PATCH_DRAWABLE_CLASS.to_str(),
            "android/graphics/drawable/NinePatchDrawable"
        );
        assert_eq!(NINE_PATCH_CREATE_NAME.to_str(), "nativeCreate");
        assert_eq!(
            NINE_PATCH_CREATE_FROM_PATH_SIG.to_str(),
            "(Ljava/lang/String;)J"
        );
        assert_eq!(NINE_PATCH_CREATE_FROM_CHUNK_SIG.to_str(), "([BJ)J");
        assert_eq!(NINE_PATCH_SET_TINT_NAME.to_str(), "nativeSetTint");
        assert_eq!(NINE_PATCH_SET_TINT_SIG.to_str(), "(JI)V");

        assert_ne!(DRAWABLE_HANDLE_SENTINEL, 0);

        assert_ne!(DRAWABLE_CONTAINER_HANDLE_SENTINEL, 0);
        assert_ne!(DRAWABLE_CONTAINER_HANDLE_SENTINEL, DRAWABLE_HANDLE_SENTINEL);
    }

    #[test]
    fn imm_native_name_sig_and_class_match_api_impl_dex() {
        assert_eq!(
            INPUT_METHOD_MANAGER_CLASS.to_str(),
            "android/view/inputmethod/InputMethodManager"
        );
        assert_eq!(IMM_NATIVE_INIT_NAME.to_str(), "nativeInit");
        assert_eq!(IMM_NATIVE_INIT_SIG.to_str(), "()J");
        assert_eq!(
            IMM_NATIVE_HIDE_SOFT_INPUT_NAME.to_str(),
            "nativeHideSoftInput"
        );
        assert_eq!(IMM_NATIVE_HIDE_SOFT_INPUT_SIG.to_str(), "(J)V");
        assert_eq!(
            IMM_NATIVE_SHOW_SOFT_INPUT_NAME.to_str(),
            "nativeShowSoftInput"
        );
        assert_eq!(
            IMM_NATIVE_SHOW_SOFT_INPUT_SIG.to_str(),
            "(JJLandroid/view/inputmethod/InputConnection;I)Z"
        );
    }

    #[test]
    fn view_group_native_name_sig_and_class_match_view_group_java() {
        assert_eq!(VIEW_GROUP_CLASS.to_str(), "android/view/ViewGroup");
        assert_eq!(VIEW_GROUP_NATIVE_ADD_VIEW_NAME.to_str(), "native_addView");
        assert_eq!(
            VIEW_GROUP_NATIVE_ADD_VIEW_SIG.to_str(),
            "(JJILandroid/view/ViewGroup$LayoutParams;)V"
        );

        assert_eq!(
            VIEW_GROUP_NATIVE_REMOVE_VIEW_NAME.to_str(),
            "native_removeView"
        );
        assert_eq!(VIEW_GROUP_NATIVE_REMOVE_VIEW_SIG.to_str(), "(JJ)V");
    }

    fn build_arsc_package(package_id: u32) -> Vec<u8> {
        fn u16(v: &mut Vec<u8>, x: u16) {
            v.extend_from_slice(&x.to_le_bytes());
        }
        fn u32(v: &mut Vec<u8>, x: u32) {
            v.extend_from_slice(&x.to_le_bytes());
        }

        let mut pool = Vec::new();
        u16(&mut pool, 0x0001);
        u16(&mut pool, 28);
        u32(&mut pool, 28);
        u32(&mut pool, 0);
        u32(&mut pool, 0);
        u32(&mut pool, 0);
        u32(&mut pool, 28);
        u32(&mut pool, 0);

        let mut type_chunk = Vec::new();
        u16(&mut type_chunk, 0x0201);
        u16(&mut type_chunk, 20);
        u32(&mut type_chunk, 40);
        type_chunk.push(1);
        type_chunk.push(0);
        u16(&mut type_chunk, 0);
        u32(&mut type_chunk, 1);
        u32(&mut type_chunk, 24);
        u32(&mut type_chunk, 0);
        u16(&mut type_chunk, 8);
        u16(&mut type_chunk, 0);
        u32(&mut type_chunk, 0);
        u16(&mut type_chunk, 8);
        type_chunk.push(0);
        type_chunk.push(0x10);
        u32(&mut type_chunk, 7);

        const PKG_HEADER: usize = 284;
        let mut pkg = Vec::new();
        u16(&mut pkg, 0x0200);
        u16(&mut pkg, PKG_HEADER as u16);
        u32(&mut pkg, (PKG_HEADER + type_chunk.len()) as u32);
        u32(&mut pkg, package_id);
        pkg.resize(pkg.len() + 256, 0);
        u32(&mut pkg, 0);
        u32(&mut pkg, 0);
        u32(&mut pkg, 0);
        u32(&mut pkg, 0);
        debug_assert_eq!(pkg.len(), PKG_HEADER);
        pkg.extend_from_slice(&type_chunk);

        let mut table = Vec::new();
        u16(&mut table, 0x0002);
        u16(&mut table, 12);
        u32(&mut table, (12 + pool.len() + pkg.len()) as u32);
        u32(&mut table, 1);
        table.extend_from_slice(&pool);
        table.extend_from_slice(&pkg);
        table
    }

    #[test]
    fn arsc_byte_cache_loads_once_and_reuses_the_backing_storage() {
        let cache = OnceLock::new();
        let loads = std::cell::Cell::new(0usize);
        let first = cached_arsc_bytes(&cache, || {
            loads.set(loads.get() + 1);
            Some(vec![1, 2, 3, 4])
        })
        .expect("first load succeeds");
        let repeated = cached_arsc_bytes(&cache, || panic!("cached table must not reload"))
            .expect("cached load succeeds");

        assert_eq!(loads.get(), 1);
        assert!(std::ptr::eq(first.as_ptr(), repeated.as_ptr()));
    }

    #[test]
    fn framework_arsc_bytes_serve_the_framework_res_apk_table() {
        use std::io::Write;

        let dir = std::env::temp_dir().join(format!(
            "eclipse-fwarsc-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp framework dir");

        let arsc = build_arsc_package(0x01);
        let apk_bytes = {
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zw.start_file("resources.arsc", opts).expect("zip entry");
            zw.write_all(&arsc).expect("write arsc");
            zw.finish().expect("finish zip").into_inner()
        };
        let framework_res_apk = dir.join("framework-res.apk");
        std::fs::write(&framework_res_apk, &apk_bytes).expect("write framework-res.apk");

        let cache = OnceLock::new();
        let bytes =
            framework_arsc_bytes(&cache, &framework_res_apk).expect("the framework table loads");
        let table = crate::apk::arsc::parse_arsc(bytes).expect("framework arsc parses");
        assert_eq!(
            table.package_ids(),
            vec![0x01],
            "high-byte-0x01 id must be served by the framework table (package 0x01)"
        );
        let v = table
            .resource_value(0x0101_0000, &ResConfig::default())
            .expect("framework entry resolves");
        assert_eq!(
            v.data, 7,
            "resolved from the framework table, not the app table"
        );

        let repeated = framework_arsc_bytes(&cache, &framework_res_apk)
            .expect("framework table remains available");
        let reused = std::ptr::eq(bytes.as_ptr(), repeated.as_ptr());

        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            reused,
            "resource resolution must borrow one cached table instead of cloning megabytes per attribute"
        );
    }

    #[test]
    fn installed_framework_declares_strictly_registered_natives() {
        let framework = match crate::runtime::find_framework() {
            Ok(paths) => paths,
            Err(error) => {
                eprintln!("SKIP: Android framework jar unavailable on this host ({error})");
                return;
            }
        };
        let mut jar = crate::apk::Apk::open(&framework.api_impl_jar)
            .unwrap_or_else(|e| panic!("cannot open {}: {e}", framework.api_impl_jar.display()));
        let mut dex_files = Vec::new();
        for index in 1.. {
            let entry = if index == 1 {
                "classes.dex".to_string()
            } else {
                format!("classes{index}.dex")
            };
            match jar.read_entry(&entry) {
                Ok(bytes) => dex_files.push(bytes),
                Err(crate::apk::ApkError::EntryMissing(_)) => break,
                Err(e) => panic!("cannot read {entry}: {e}"),
            }
        }
        assert!(
            !dex_files.is_empty(),
            "{} has no classes.dex",
            framework.api_impl_jar.display()
        );
        let dialog_names: Vec<&JNIStr> = dialogs::DIALOG_NATIVES
            .iter()
            .map(|&(name, _)| name)
            .collect();
        let alert_dialog_names: Vec<&JNIStr> = dialogs::ALERT_DIALOG_NATIVES
            .iter()
            .map(|&(name, _)| name)
            .collect();
        let strictly_registered: [(&JNIStr, &[&JNIStr]); 9] = [
            (
                CONNECTIVITY_MANAGER_CLASS,
                &[
                    CM_REGISTER_NETWORK_CALLBACK_NAME,
                    CM_IS_ACTIVE_NETWORK_METERED_NAME,
                    CM_NATIVE_GET_NETWORK_AVAILABLE_NAME,
                ],
            ),
            (
                WINDOW_CLASS,
                &[
                    WINDOW_SET_JOBJECT_NAME,
                    WINDOW_SET_TITLE_NAME,
                    WINDOW_SET_LAYOUT_NAME,
                    WINDOW_SET_WIDGET_AS_ROOT_NAME,
                    WINDOW_REMOVE_GTK_BACKGROUND_NAME,
                    WINDOW_SET_SCREEN_BRIGHTNESS_NAME,
                    WINDOW_TAKE_INPUT_QUEUE_NAME,
                ],
            ),
            (
                PROCESS_CLASS,
                &[
                    PROCESS_GET_ELAPSED_CPU_TIME_NAME,
                    PROCESS_IS_64_BIT_NAME,
                    PROCESS_GET_THREAD_PRIORITY_NAME,
                ],
            ),
            (
                SYSTEM_CLOCK_CLASS,
                &[
                    ELAPSED_REALTIME_NAME,
                    ELAPSED_REALTIME_NANOS_NAME,
                    UPTIME_MILLIS_NAME,
                    CURRENT_THREAD_TIME_MILLIS_NAME,
                ],
            ),
            (ACTIVITY_CLASS, &[external_intents::NATIVE_OPEN_URI_NAME]),
            (CONTEXT_CLASS, &[external_intents::NATIVE_SHARE_FILE_NAME]),
            (ASSET_MANAGER_CLASS, &[ASSET_MANAGER_DELETE_THEME_NAME]),
            (dialogs::DIALOG_CLASS, &dialog_names),
            (dialogs::ALERT_DIALOG_CLASS, &alert_dialog_names),
        ];
        for (class, names) in strictly_registered {
            for name in names {
                let name = name.to_str();
                assert!(
                    dex_files.iter().any(|dex| dex_declares_string(dex, &name)),
                    "{} native {name:?} is registered strictly by Eclipse but not declared by {} \
                     (RegisterNatives would abort the framework lifecycle)",
                    class.to_str(),
                    framework.api_impl_jar.display()
                );
            }
        }
    }

    fn dex_declares_string(dex: &[u8], name: &str) -> bool {
        assert!(
            name.is_ascii() && name.len() < 0x80,
            "dex string check supports short ASCII names only: {name:?}"
        );
        let mut item = Vec::with_capacity(name.len() + 2);
        item.push(name.len() as u8);
        item.extend_from_slice(name.as_bytes());
        item.push(0);
        dex.windows(item.len()).any(|window| window == item)
    }

    #[test]
    fn path_builder_reused_after_draw_consumes_the_drawn_path_without_duplicating() {
        let builder = path_registry::allocate(path_registry::PathGeometry::default())
            .expect("allocate builder");
        path_registry::with_path(builder, |g| {
            g.move_to(0.0, 0.0);
            g.line_to(1.0, 1.0);
        })
        .expect("record builder ops");
        let drawn = path_clone_handle(builder, "native_create_path");

        let rebuilt = path_rebuild_builder(drawn, builder);

        assert_eq!(rebuilt, builder, "the live builder is reused, not replaced");
        assert_eq!(
            path_registry::get(drawn),
            Err(path_registry::PathRegistryError::StaleHandle),
            "the consumed path slot is released"
        );
        assert_eq!(
            path_registry::get(builder)
                .expect("builder still live")
                .verbs,
            vec![path_registry::Verb::MoveTo, path_registry::Verb::LineTo],
            "the builder holds the path geometry exactly once"
        );
        path_registry::free(builder).expect("free builder");
    }

    #[test]
    fn path_builder_from_a_copied_path_takes_its_geometry_and_releases_it() {
        let copied = path_registry::allocate(path_registry::PathGeometry::default())
            .expect("allocate copied path");
        path_registry::with_path(copied, |g| g.move_to(4.0, 5.0)).expect("record path op");

        let builder = path_rebuild_builder(copied, 0);

        assert_ne!(builder, 0, "a fresh builder is allocated");
        assert_ne!(builder, copied, "the builder is a new slot");
        assert_eq!(
            path_registry::get(copied),
            Err(path_registry::PathRegistryError::StaleHandle),
            "the copied path slot is released"
        );
        assert_eq!(
            path_registry::get(builder).expect("builder live").points,
            vec![4.0, 5.0]
        );
        path_registry::free(builder).expect("free builder");
    }

    #[test]
    fn click_listener_can_reenter_view_natives_while_perform_click_runs() {
        let (probe_result, probe) = std::sync::mpsc::channel();
        let clicked = fake_jvm::with_env(|env| {
            let button = fake_jvm::new_object(env);
            let handle = view_registry::allocate("android.widget.Button").expect("allocate");
            let peer = env.new_weak_ref(&button).expect("weak ref");
            view_registry::set_jobject(handle, view_registry::ViewObject::Peer(peer))
                .expect("record peer");
            fake_jvm::on_call(&button, "performClick", move || {
                let (reached, registry_reached) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let listener_native = view_registry::with_view(handle, |v| v.clickable = true);
                    reached.send(listener_native.is_ok()).ok();
                });
                let reached_in_time =
                    registry_reached.recv_timeout(std::time::Duration::from_secs(5));
                probe_result.send(reached_in_time == Ok(true)).ok();
            });
            let clicked = perform_click(env, handle);
            view_registry::free(handle).expect("free");
            clicked
        });
        assert_eq!(clicked.ok(), Some(true), "performClick reaches Java");
        assert_eq!(
            probe.try_recv(),
            Ok(true),
            "performClick must not hold the view registry while Java runs"
        );
    }

    #[test]
    fn view_constructor_records_a_peer_that_does_not_pin_the_view() {
        fake_jvm::with_env(|env| {
            let view = fake_jvm::new_object(env);
            let this = env.new_local_ref(&view).expect("receiver ref");
            let handle = view_native_constructor(
                fake_jvm::native_env(),
                this,
                JObject::null(),
                JObject::null(),
            );
            assert_ne!(handle, 0, "the constructor allocates a peer");
            assert_eq!(
                fake_jvm::take_exception(),
                None,
                "the constructor leaves no Java exception pending"
            );

            assert_eq!(
                fake_jvm::strong_refs(&view),
                0,
                "the registry keeps no strong reference to the Java view"
            );
            assert!(matches!(
                view_registry::local_jobject(env, handle),
                Ok(Ok(Some(_)))
            ));
            assert!(fake_jvm::collect(&view), "the view is collectable");
            assert!(matches!(
                view_registry::local_jobject(env, handle),
                Ok(Ok(None))
            ));
            view_registry::free(handle).expect("finalizer frees the peer");
        });
    }

    #[test]
    fn set_widget_as_root_on_an_invalid_window_leaves_the_active_root_alone() {
        let dialog_placeholder_window: jlong = 1;
        let dialog_decor = view_registry::allocate("com.android.internal.policy.DecorView")
            .expect("allocate dialog decor view");

        window_set_widget_as_root(
            fake_jvm::native_env(),
            JObject::null(),
            dialog_placeholder_window,
            dialog_decor,
        );

        assert_ne!(
            view_registry::active_root(),
            dialog_decor,
            "a content view set on an invalid window must not become the active root"
        );
        view_registry::free(dialog_decor).expect("free dialog decor view");
    }

    #[test]
    fn finished_activity_is_released_and_still_finishes_once() {
        let _lock = ACTIVITY_TRACKER_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        fake_jvm::with_env(|env| {
            let activity = fake_jvm::new_object(env);
            track_activity(env, &activity);
            assert_eq!(
                fake_jvm::strong_refs(&activity),
                1,
                "a live activity is tracked"
            );

            assert!(
                mark_activity_finished_once(env, &activity),
                "first finish runs"
            );
            assert_eq!(
                fake_jvm::strong_refs(&activity),
                0,
                "a finished activity is no longer pinned"
            );
            assert!(
                !mark_activity_finished_once(env, &activity),
                "a second queued finish is a no-op"
            );

            assert!(fake_jvm::collect(&activity));
            let other = fake_jvm::new_object(env);
            assert!(mark_activity_finished_once(env, &other));
            let tracker = TRACKED_ACTIVITIES.lock().expect("tracker");
            assert!(
                !tracker.iter().any(|entry| entry.is_collected(env)),
                "collected finished activities are pruned"
            );
        });
    }

    #[test]
    fn recreate_finish_with_a_null_window_handle_drives_the_old_activity_down() {
        let _lock = ACTIVITY_TRACKER_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        fake_jvm::with_env(|env| {
            let activity = fake_jvm::new_object(env);
            track_activity(env, &activity);
            let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            for callback in ["onPause", "onStop", "onDestroy"] {
                let calls = std::sync::Arc::clone(&calls);
                fake_jvm::on_call(&activity, callback, move || {
                    calls.lock().expect("calls").push(callback);
                });
            }
            let receiver = env.new_local_ref(&activity).expect("receiver");

            activity_native_finish(fake_jvm::native_env(), receiver, 0);

            assert_eq!(
                *calls.lock().expect("calls"),
                ["onPause", "onStop", "onDestroy"],
                "Activity.recreate's nativeFinish(0) drives the old activity down"
            );
            assert_eq!(
                fake_jvm::strong_refs(&activity),
                0,
                "the recreated activity is no longer pinned as live"
            );
            assert!(
                !mark_activity_finished_once(env, &activity),
                "the old activity is recorded as finished"
            );
        });
    }

    #[test]
    fn a_recreated_activity_tree_offers_its_new_engine_surface_view() {
        let activity_tree = || {
            let surface = view_registry::allocate(RBX_SURFACE_VIEW_CLASS).expect("surface view");
            let decor = view_registry::allocate("android.widget.FrameLayout").expect("decor");
            view_registry::with_view(decor, |state| state.children.push(surface)).expect("wire");
            (decor, surface)
        };
        let (old_decor, old_surface) = activity_tree();
        let (new_decor, new_surface) = activity_tree();
        let splash_decor = view_registry::allocate("android.widget.FrameLayout").expect("splash");

        assert_eq!(
            replacement_engine_surface_view(new_decor, Some(old_surface), None),
            Some(new_surface)
        );
        assert_eq!(
            replacement_engine_surface_view(new_decor, None, Some(old_surface)),
            Some(new_surface)
        );
        assert_eq!(
            replacement_engine_surface_view(old_decor, Some(old_surface), None),
            None
        );
        assert_eq!(
            replacement_engine_surface_view(new_decor, Some(new_surface), Some(old_surface)),
            None
        );
        assert_eq!(
            replacement_engine_surface_view(splash_decor, Some(old_surface), None),
            None
        );

        for handle in [old_decor, old_surface, new_decor, new_surface, splash_decor] {
            view_registry::free(handle).expect("free");
        }
    }

    static ENGINE_SURFACE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn a_finished_activity_is_not_handed_its_destroyed_engine_surface_again() {
        let _lock = ENGINE_SURFACE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let surface = view_registry::allocate(RBX_SURFACE_VIEW_CLASS).expect("surface view");
        let decor = view_registry::allocate("android.widget.FrameLayout").expect("decor");
        view_registry::with_view(decor, |state| state.children.push(surface)).expect("wire");
        ENGINE_SURFACE_VIEW.store(surface, std::sync::atomic::Ordering::Release);

        let dispatched = fake_jvm::with_env(destroy_engine_surface).expect("surface teardown");

        assert!(!dispatched, "the surface view has no Java peer to call");
        assert_eq!(engine_surface_view_handle(), None);
        assert_eq!(
            replacement_engine_surface_view(
                decor,
                engine_surface_view_handle(),
                retired_engine_surface_view()
            ),
            None,
            "the finished activity's tree is not offered the surface again"
        );

        RETIRED_ENGINE_SURFACE_VIEW.store(0, std::sync::atomic::Ordering::Release);
        for handle in [decor, surface] {
            view_registry::free(handle).expect("free");
        }
    }

    #[test]
    fn a_failed_engine_surface_teardown_still_stops_and_destroys_the_activity() {
        let _lock = ENGINE_SURFACE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let surface = view_registry::allocate(RBX_SURFACE_VIEW_CLASS).expect("surface view");
        ENGINE_SURFACE_VIEW.store(surface, std::sync::atomic::Ordering::Release);

        fake_jvm::with_env(|env| {
            let activity = fake_jvm::new_object(env);
            let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            for callback in ["onPause", "onStop", "onDestroy"] {
                let calls = std::sync::Arc::clone(&calls);
                fake_jvm::on_call(&activity, callback, move || {
                    calls.lock().expect("calls").push(callback);
                });
            }
            fake_jvm::on_call(&activity, "getWindow", || {
                fake_jvm::throw("java/lang/IllegalStateException");
            });

            let outcome = drive_activity_down_lifecycle(env, &activity);

            assert!(outcome.is_err(), "the surface teardown failure is reported");
            assert_eq!(
                *calls.lock().expect("calls"),
                ["onPause", "onStop", "onDestroy"]
            );
        });

        ENGINE_SURFACE_VIEW.store(0, std::sync::atomic::Ordering::Release);
        view_registry::free(surface).expect("free surface");
    }

    #[test]
    fn shutdown_snapshot_marks_live_activities_finished_without_pinning_them() {
        let _lock = ACTIVITY_TRACKER_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        fake_jvm::with_env(|env| {
            let activity = fake_jvm::new_object(env);
            track_activity(env, &activity);

            let (snapshot, error) = snapshot_live_activities_for_shutdown(env);
            assert!(error.is_none());
            assert!(snapshot
                .iter()
                .any(|live| env.is_same_object(live, &activity).unwrap_or(false)));
            assert_eq!(fake_jvm::strong_refs(&activity), 0);
            assert!(
                !mark_activity_finished_once(env, &activity),
                "an activity driven down by shutdown is not finished again"
            );
        });
    }

    fn activity_with_content<'local>(
        env: &mut Env<'local>,
        native_window: jlong,
    ) -> (JObject<'local>, jlong) {
        let activity = fake_jvm::new_object(env);
        let window = fake_jvm::new_object(env);
        let decor = fake_jvm::new_object(env);
        let root = view_registry::allocate("android.widget.FrameLayout").expect("allocate root");
        fake_jvm::on_call_return(&activity, "getWindow", &window);
        fake_jvm::on_call_return(&window, "getDecorView", &decor);
        fake_jvm::set_long_field(&decor, "widget", root);
        let this = env.new_local_ref(&window).expect("window ref");
        window_set_widget_as_root(fake_jvm::native_env(), this, native_window, root);
        (activity, root)
    }

    fn web_view_in(root: jlong) -> jlong {
        let web = view_registry::allocate("android.webkit.WebView").expect("allocate WebView");
        view_registry::attach_child(root, web, None).expect("attach WebView");
        web
    }

    #[test]
    fn finishing_an_activity_hides_only_the_web_views_in_its_own_content() {
        let _lock = ACTIVITY_TRACKER_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _windows = window_registry::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let native_window = window_registry::allocate().expect("allocate window");
        fake_jvm::with_env(|env| {
            let (splash, splash_root) = activity_with_content(env, native_window);
            let (main, main_root) = activity_with_content(env, native_window);
            let splash_web = web_view_in(splash_root);
            let main_web = web_view_in(main_root);

            let this = env.new_local_ref(&splash).expect("splash ref");
            activity_native_finish(fake_jvm::native_env(), this, native_window);
            let after_splash = (
                view_registry::is_shown(splash_web),
                view_registry::is_shown(main_web),
            );
            let this = env.new_local_ref(&main).expect("main ref");
            activity_native_finish(fake_jvm::native_env(), this, native_window);
            let after_main = view_registry::is_shown(main_web);

            assert_eq!(
                after_splash,
                (false, true),
                "finishing the splash activity hides its own WebViews and keeps the WebViews of \
                 the activity that replaced it in the shared window"
            );
            assert!(
                !after_main,
                "finishing the last activity hides its WebViews"
            );
            for handle in [splash_web, main_web, splash_root, main_root] {
                view_registry::free(handle).expect("free view");
            }
        });
        window_registry::free(native_window).expect("free window");
    }
}
