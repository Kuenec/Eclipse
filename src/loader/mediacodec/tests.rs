use super::*;

use crate::loader::log_capture::formatted_log;
use crate::loader::native_provider::EclipseNativeProvider;
use crate::loader::resolve::SymbolProvider;

type CreateCodecFn = unsafe extern "C" fn(*const c_char) -> *mut c_void;
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
fn media_ndk_natives_return_unavailable_sentinels() {
    assert!(unsafe { eclipse_amediacodec_createdecoderbytype(std::ptr::null()) }.is_null());

    assert!(unsafe { eclipse_amediacodec_createencoderbytype(std::ptr::null()) }.is_null());
    assert!(eclipse_amediaformat_new().is_null());

    assert!(unsafe { eclipse_amediacodec_getoutputformat(std::ptr::null_mut()) }.is_null());

    unsafe {
        assert_eq!(
            eclipse_amediacodec_start(std::ptr::null_mut()),
            AMEDIA_ERROR_UNSUPPORTED
        );
        assert_eq!(
            eclipse_amediacodec_stop(std::ptr::null_mut()),
            AMEDIA_ERROR_UNSUPPORTED
        );
        assert_eq!(
            eclipse_amediacodec_flush(std::ptr::null_mut()),
            AMEDIA_ERROR_UNSUPPORTED
        );
        assert_eq!(
            eclipse_amediacodec_configure(
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0
            ),
            AMEDIA_ERROR_UNSUPPORTED
        );
    }

    assert_eq!(AMEDIA_ERROR_UNSUPPORTED, -10002);

    unsafe {
        assert!(eclipse_amediacodec_dequeueinputbuffer(std::ptr::null_mut(), 0) < 0);
        assert!(
            eclipse_amediacodec_dequeueoutputbuffer(std::ptr::null_mut(), std::ptr::null_mut(), 0)
                < 0
        );
    }

    unsafe {
        assert!(!eclipse_amediaformat_getint32(
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null_mut()
        ));
        assert!(!eclipse_amediaformat_getbuffer(
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut()
        ));
    }

    let s = unsafe { eclipse_amediaformat_tostring(std::ptr::null_mut()) };
    assert!(!s.is_null(), "toString must never return NULL");

    assert_eq!(unsafe { *s }, 0, "toString returns an empty string");
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
    let log = formatted_log("mediandk=debug", || {
        assert!(unsafe { create(c"video/avc".as_ptr()) }.is_null());
    });
    let line = log
        .lines()
        .find(|line| line.contains("AMediaCodec_createDecoderByType"))
        .unwrap_or_else(|| panic!("no createDecoderByType line in {log:?}"));
    assert!(line.contains("DEBUG mediandk:"), "{line}");
    assert!(line.contains("mime=Some(\"video/avc\")"), "{line}");
    assert!(line.contains("codec=0x0"), "{line}");
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
        assert_eq!(status, AMEDIA_ERROR_UNSUPPORTED);
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
        "status=-10002",
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
