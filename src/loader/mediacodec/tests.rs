use std::ffi::c_void;

use super::*;

use crate::loader::log_capture::formatted_log;
use crate::loader::native_provider::EclipseNativeProvider;
use crate::loader::resolve::SymbolProvider;

type CreateCodecFn = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type DeleteCodecFn = unsafe extern "C" fn(*mut c_void) -> MediaStatus;
type QueueInputFn =
    unsafe extern "C" fn(*mut c_void, usize, libc::off_t, usize, u64, u32) -> MediaStatus;
type SetBufferFn = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_void, usize);

fn provider_function(name: &str) -> *const () {
    EclipseNativeProvider::with_bionic_natives()
        .resolve(name)
        .unwrap_or_else(|| panic!("{name} must be an Eclipse native"))
        .addr as usize as *const ()
}

#[test]
fn register_natives_count_matches_and_every_name_resolves_through_the_provider() {
    let mut registered = Vec::new();
    register_natives(|name, addr| registered.push((name, addr)));
    assert_eq!(registered.len(), MEDIA_NDK_NATIVE_COUNT);

    let provider = EclipseNativeProvider::with_bionic_natives();
    for (name, addr) in &registered {
        assert_ne!(*addr, 0, "{name} must have an address");
        assert_eq!(
            provider.resolve(name).map(|sym| sym.addr),
            Some(*addr),
            "{name} must resolve to the media native"
        );
    }

    let mut names: Vec<_> = registered.iter().map(|(name, _)| *name).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), MEDIA_NDK_NATIVE_COUNT, "names are unique");
}

#[test]
fn media_status_codes_match_the_ndk() {
    assert_eq!(AMEDIA_OK, 0);
    assert_eq!(AMEDIA_ERROR_UNKNOWN, -10000);
    assert_eq!(AMEDIA_ERROR_UNSUPPORTED, -10002);
    assert_eq!(AMEDIA_ERROR_INVALID_OBJECT, -10003);
    assert_eq!(AMEDIA_ERROR_INVALID_PARAMETER, -10004);
    assert_eq!(AMEDIA_ERROR_INVALID_OPERATION, -10005);
}

#[test]
fn amediaformat_key_data_objects_hold_the_public_key_strings() {
    let cases = [
        ("AMEDIAFORMAT_KEY_MIME", "mime"),
        ("AMEDIAFORMAT_KEY_WIDTH", "width"),
        ("AMEDIAFORMAT_KEY_HEIGHT", "height"),
        ("AMEDIAFORMAT_KEY_BIT_RATE", "bitrate"),
        ("AMEDIAFORMAT_KEY_SAMPLE_RATE", "sample-rate"),
        ("AMEDIAFORMAT_KEY_I_FRAME_INTERVAL", "i-frame-interval"),
    ];
    let p = EclipseNativeProvider::with_bionic_natives();
    for (name, want) in cases {
        let addr = p.resolve(name).expect("key registered").addr;
        assert!(addr != 0, "{name} data symbol must be non-null");

        let strp = unsafe { *(addr as *const *const c_char) };
        assert!(!strp.is_null(), "{name} value (the char*) must be non-null");

        let got = unsafe { std::ffi::CStr::from_ptr(strp) };
        assert_eq!(got.to_str().unwrap(), want, "{name} == \"{want}\"");
    }
}

#[test]
fn create_decoder_by_type_logs_the_requested_mime_under_mediandk() {
    let create = unsafe {
        std::mem::transmute::<*const (), CreateCodecFn>(provider_function(
            "AMediaCodec_createDecoderByType",
        ))
    };
    let delete = unsafe {
        std::mem::transmute::<*const (), DeleteCodecFn>(provider_function("AMediaCodec_delete"))
    };
    let mut codec = std::ptr::null_mut();
    let log = formatted_log("mediandk=debug", || {
        codec = unsafe { create(c"video/avc".as_ptr()) };
    });
    if !codec.is_null() {
        assert_eq!(unsafe { delete(codec) }, AMEDIA_OK);
    }
    let line = log
        .lines()
        .find(|line| {
            line.contains("DEBUG mediandk:") && line.contains("AMediaCodec_createDecoderByType")
        })
        .unwrap_or_else(|| panic!("no createDecoderByType debug line in {log:?}"));
    assert!(line.contains("mime=Some(\"video/avc\")"), "{line}");
    assert!(line.contains(&format!("codec={codec:?}")), "{line}");
}

#[test]
fn per_buffer_calls_log_index_size_pts_and_flags_only_at_trace() {
    let queue = unsafe {
        std::mem::transmute::<*const (), QueueInputFn>(provider_function(
            "AMediaCodec_queueInputBuffer",
        ))
    };
    let call = || {
        let status = unsafe { queue(std::ptr::null_mut(), 3, 0, 4096, 33_366, 1) };
        assert_eq!(status, AMEDIA_ERROR_INVALID_OBJECT);
    };

    let trace = formatted_log("mediandk=trace", call);
    let line = trace
        .lines()
        .find(|line| line.contains("AMediaCodec_queueInputBuffer"))
        .unwrap_or_else(|| panic!("no queueInputBuffer line in {trace:?}"));
    for field in [
        "TRACE mediandk:",
        "index=3",
        "size=4096",
        "pts_us=33366",
        "flags=1",
        "status=-10003",
    ] {
        assert!(line.contains(field), "{field} missing from {line}");
    }

    assert_eq!(formatted_log("mediandk=debug", call), "");
}

#[test]
fn format_setters_log_the_key_and_a_buffer_length_but_never_its_bytes() {
    let set_buffer = unsafe {
        std::mem::transmute::<*const (), SetBufferFn>(provider_function("AMediaFormat_setBuffer"))
    };
    let payload = b"codec-private-bytes";
    let log = formatted_log("mediandk=debug", || unsafe {
        set_buffer(
            std::ptr::null_mut(),
            c"csd-0".as_ptr(),
            payload.as_ptr().cast(),
            payload.len(),
        );
    });
    assert!(log.contains("AMediaFormat_setBuffer"), "{log}");
    assert!(log.contains("key=Some(\"csd-0\")"), "{log}");
    assert!(log.contains("size=19"), "{log}");
    assert!(!log.contains("codec-private"), "{log}");
}

#[test]
fn missing_video_decoders_are_a_problem_with_their_fix() {
    use avcodec::{BackendUnavailable, Codec};

    assert_eq!(video_problem(Ok(&Codec::ALL)), None);
    assert_eq!(
        video_problem(Ok(&[Codec::Aac, Codec::Mp3, Codec::Opus])),
        Some(
            "H.264 and HEVC videos in games will not play: H.264/HEVC need \
             org.freedesktop.Platform.codecs-extra//26.08-extra; install it with flatpak install \
             flathub org.freedesktop.Platform.codecs-extra//26.08-extra."
                .to_owned()
        )
    );
    let missing = BackendUnavailable::NotFound {
        tried: vec!["libavcodec.so.62", "libavcodec.so.63"],
    };
    assert_eq!(
        video_problem(Err(&missing)),
        Some(format!("{missing}.")),
        "without FFmpeg the reason names the fix"
    );
}
