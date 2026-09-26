use super::proto::Encoder;

const MANUFACTURER: &str = "Google";
const BRAND: &str = "google";
const MODEL: &str = "Chromebook";
const DEVICE: &str = "brya_cheets";
const PRODUCT: &str = "brya";
const HARDWARE: &str = "cheets";
const BOOTLOADER: &str = "unknown";
const RADIO: &str = "unknown";
const BUILD_ID: &str = "R128-15964.59.0";
const INCREMENTAL: &str = "12587421";
const RELEASE: &str = "13";
pub(super) const SDK_INT: i32 = 33;
const CLIENT_ID: &str = "android-google";
pub(super) const PLAY_SERVICES_VERSION: i32 = 250_632_035;
const PLAY_STORE_VERSION_CODE: i32 = 84_411_930;
const PLAY_STORE_VERSION_NAME: &str = "44.1.19-31 [0] [PR] 702116218";
pub(super) const LOCALE: &str = "en_US";
pub(super) const LANGUAGE: &str = "en";
pub(super) const COUNTRY: &str = "us";
const TIME_ZONE: &str = "America/New_York";

const NATIVE_PLATFORMS: [&str; 5] = ["x86_64", "x86", "arm64-v8a", "armeabi-v7a", "armeabi"];
const GLES_3_2: i32 = 0x0003_0002;
const TOUCHSCREEN_FINGER: i32 = 3;
const KEYBOARD_QWERTY: i32 = 2;
const NAVIGATION_NONAV: i32 = 1;
const SCREENLAYOUT_SIZE_XLARGE_LONG_YES: i32 = 0x24;
const SCREEN_WIDTH: i32 = 1920;
const SCREEN_HEIGHT: i32 = 1080;
const SCREEN_DENSITY: i32 = 240;
const SMALLEST_SCREEN_WIDTH_DP: i32 = 720;
const TOTAL_MEMORY_BYTES: i64 = 8 * 1024 * 1024 * 1024;
const CPU_CORES: i32 = 8;

const SHARED_LIBRARIES: [&str; 12] = [
    "android.ext.services",
    "android.ext.shared",
    "android.hidl.base-V1.0-java",
    "android.hidl.manager-V1.0-java",
    "android.test.base",
    "android.test.mock",
    "android.test.runner",
    "com.android.location.provider",
    "com.android.media.remotedisplay",
    "com.android.mediadrm.signer",
    "javax.obex",
    "org.apache.http.legacy",
];

const FEATURES: [&str; 44] = [
    "android.hardware.audio.output",
    "android.hardware.bluetooth",
    "android.hardware.bluetooth_le",
    "android.hardware.camera",
    "android.hardware.camera.any",
    "android.hardware.camera.front",
    "android.hardware.faketouch",
    "android.hardware.gamepad",
    "android.hardware.location",
    "android.hardware.location.network",
    "android.hardware.microphone",
    "android.hardware.opengles.aep",
    "android.hardware.ram.normal",
    "android.hardware.screen.landscape",
    "android.hardware.screen.portrait",
    "android.hardware.sensor.accelerometer",
    "android.hardware.touchscreen",
    "android.hardware.touchscreen.multitouch",
    "android.hardware.touchscreen.multitouch.distinct",
    "android.hardware.touchscreen.multitouch.jazzhand",
    "android.hardware.type.pc",
    "android.hardware.usb.accessory",
    "android.hardware.usb.host",
    "android.hardware.wifi",
    "android.software.activities_on_secondary_displays",
    "android.software.app_widgets",
    "android.software.autofill",
    "android.software.backup",
    "android.software.companion_device_setup",
    "android.software.cts",
    "android.software.device_admin",
    "android.software.file_based_encryption",
    "android.software.freeform_window_management",
    "android.software.home_screen",
    "android.software.input_methods",
    "android.software.managed_users",
    "android.software.midi",
    "android.software.picture_in_picture",
    "android.software.print",
    "android.software.webview",
    "com.google.android.feature.GOOGLE_BUILD",
    "com.google.android.feature.GOOGLE_EXPERIENCE",
    "org.chromium.arc",
    "org.chromium.arc.device_management",
];

const VERSIONED_FEATURES: [(&str, i32); 3] = [
    ("android.hardware.vulkan.compute", 0),
    ("android.hardware.vulkan.level", 1),
    ("android.hardware.vulkan.version", 0x0040_3000),
];

const LOCALES: [&str; 16] = [
    "en_US", "en_GB", "de_DE", "es_ES", "es_US", "fr_FR", "it_IT", "ja_JP", "ko_KR", "nl_NL",
    "pl_PL", "pt_BR", "ru_RU", "tr_TR", "zh_CN", "zh_TW",
];

const GL_EXTENSIONS: [&str; 24] = [
    "GL_ANDROID_extension_pack_es31a",
    "GL_EXT_color_buffer_float",
    "GL_EXT_color_buffer_half_float",
    "GL_EXT_copy_image",
    "GL_EXT_debug_marker",
    "GL_EXT_disjoint_timer_query",
    "GL_EXT_draw_buffers_indexed",
    "GL_EXT_geometry_shader",
    "GL_EXT_gpu_shader5",
    "GL_EXT_shader_io_blocks",
    "GL_EXT_tessellation_shader",
    "GL_EXT_texture_border_clamp",
    "GL_EXT_texture_buffer",
    "GL_EXT_texture_cube_map_array",
    "GL_EXT_texture_filter_anisotropic",
    "GL_EXT_texture_format_BGRA8888",
    "GL_EXT_texture_sRGB_decode",
    "GL_KHR_debug",
    "GL_KHR_texture_compression_astc_ldr",
    "GL_OES_EGL_image",
    "GL_OES_EGL_image_external",
    "GL_OES_compressed_ETC1_RGB8_texture",
    "GL_OES_texture_float_linear",
    "GL_OES_vertex_array_object",
];

pub(super) fn auth_user_agent() -> String {
    format!("GoogleAuth/1.4 ({DEVICE} {BUILD_ID})")
}

pub(super) fn store_user_agent() -> String {
    format!(
        "Android-Finsky/{PLAY_STORE_VERSION_NAME} (api=3,versionCode={PLAY_STORE_VERSION_CODE},\
         sdk={SDK_INT},device={DEVICE},hardware={HARDWARE},product={PRODUCT},\
         platformVersionRelease={RELEASE},model={MODEL},buildId={BUILD_ID},isWideScreen=0,\
         supportedAbis={})",
        NATIVE_PLATFORMS.join(";")
    )
}

fn fingerprint() -> String {
    format!("{BRAND}/{PRODUCT}/{DEVICE}:{RELEASE}/{BUILD_ID}/{INCREMENTAL}:user/release-keys")
}

fn configuration() -> Encoder {
    let mut config = Encoder::default();
    config
        .int32(1, TOUCHSCREEN_FINGER)
        .int32(2, KEYBOARD_QWERTY)
        .int32(3, NAVIGATION_NONAV)
        .int32(4, SCREENLAYOUT_SIZE_XLARGE_LONG_YES)
        .bool(5, true)
        .bool(6, false)
        .int32(7, SCREEN_DENSITY)
        .int32(8, GLES_3_2);
    for library in SHARED_LIBRARIES {
        config.string(9, library);
    }
    for feature in FEATURES {
        config.string(10, feature);
    }
    for (feature, _) in VERSIONED_FEATURES {
        config.string(10, feature);
    }
    for platform in NATIVE_PLATFORMS {
        config.string(11, platform);
    }
    config.int32(12, SCREEN_WIDTH).int32(13, SCREEN_HEIGHT);
    for locale in LOCALES {
        config.string(14, locale);
    }
    for extension in GL_EXTENSIONS {
        config.string(15, extension);
    }
    config
        .int32(16, 0)
        .int32(18, SMALLEST_SCREEN_WIDTH_DP)
        .int32(19, 0)
        .int64(20, TOTAL_MEMORY_BYTES)
        .int32(21, CPU_CORES);
    for feature in FEATURES {
        let mut entry = Encoder::default();
        entry.string(1, feature).int32(2, 0);
        config.message(26, &entry);
    }
    for (feature, version) in VERSIONED_FEATURES {
        let mut entry = Encoder::default();
        entry.string(1, feature).int32(2, version);
        config.message(26, &entry);
    }
    config
}

pub(super) fn checkin_request(now_unix_seconds: i64) -> Vec<u8> {
    let mut build = Encoder::default();
    build
        .string(1, &fingerprint())
        .string(2, HARDWARE)
        .string(3, BRAND)
        .string(4, RADIO)
        .string(5, BOOTLOADER)
        .string(6, CLIENT_ID)
        .int64(7, now_unix_seconds)
        .int32(8, PLAY_SERVICES_VERSION)
        .string(9, DEVICE)
        .int32(10, SDK_INT)
        .string(11, MODEL)
        .string(12, MANUFACTURER)
        .string(13, PRODUCT)
        .bool(14, false);

    let mut checkin = Encoder::default();
    checkin
        .message(1, &build)
        .int64(2, 0)
        .string(6, "")
        .string(7, "")
        .string(8, "mobile-notroaming")
        .int32(9, 0);

    let mut request = Encoder::default();
    request
        .int64(2, 0)
        .message(4, &checkin)
        .string(6, LOCALE)
        .string(12, TIME_ZONE)
        .int32(14, 3)
        .message(18, &configuration())
        .int32(20, 0);
    request.into_bytes()
}

pub(super) fn upload_config_request() -> Vec<u8> {
    let mut request = Encoder::default();
    request.message(1, &configuration());
    request.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::super::proto::{
        bytes_field, message_at, repeated_bytes, string_field, varint_field,
    };
    use super::*;

    fn strings(message: &[u8], field: u32) -> Vec<&str> {
        repeated_bytes(message, field)
            .unwrap()
            .into_iter()
            .map(|value| std::str::from_utf8(value).unwrap())
            .collect()
    }

    #[test]
    fn device_prefers_x86_64_and_reports_gles_3_2() {
        let config = configuration().into_bytes();
        assert_eq!(
            strings(&config, 11),
            ["x86_64", "x86", "arm64-v8a", "armeabi-v7a", "armeabi"]
        );
        assert_eq!(varint_field(&config, 8).unwrap(), Some(0x0003_0002));
        assert!(strings(&config, 10).contains(&"android.hardware.type.pc"));
        let declared = repeated_bytes(&config, 26).unwrap().len();
        assert_eq!(declared, strings(&config, 10).len());
        assert!(store_user_agent().contains("supportedAbis=x86_64;x86;arm64-v8a"));
    }

    #[test]
    fn checkin_request_carries_build_locale_and_the_same_configuration() {
        let request = checkin_request(1_790_000_000);
        let build = message_at(&request, &[4, 1]).unwrap().unwrap();
        assert_eq!(varint_field(build, 10).unwrap(), Some(33));
        assert_eq!(varint_field(build, 7).unwrap(), Some(1_790_000_000));
        assert_eq!(string_field(build, 9).unwrap(), Some(DEVICE));
        assert!(string_field(build, 1)
            .unwrap()
            .unwrap()
            .contains(":13/R128-15964.59.0/"));
        assert_eq!(string_field(&request, 6).unwrap(), Some("en_US"));
        assert_eq!(varint_field(&request, 14).unwrap(), Some(3));
        assert_eq!(
            bytes_field(&request, 18).unwrap(),
            Some(configuration().into_bytes().as_slice())
        );
        assert_eq!(
            bytes_field(&upload_config_request(), 1).unwrap(),
            Some(configuration().into_bytes().as_slice())
        );
    }
}
