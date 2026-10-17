use super::*;

use crate::loader::log_capture::formatted_log;
use crate::loader::native_provider::EclipseNativeProvider;
use crate::loader::resolve::SymbolProvider;

type NewFn = unsafe extern "C" fn() -> *mut c_void;
type DeleteFn = unsafe extern "C" fn(*mut c_void) -> MediaStatus;
type SetInt32Fn = unsafe extern "C" fn(*mut c_void, *const c_char, i32);
type GetInt32Fn = unsafe extern "C" fn(*mut c_void, *const c_char, *mut i32) -> bool;

fn provider_address(name: &str) -> u64 {
    EclipseNativeProvider::with_bionic_natives()
        .resolve(name)
        .unwrap_or_else(|| panic!("{name} must be an Eclipse native"))
        .addr
}

fn key_symbol(name: &str) -> *const c_char {
    unsafe { *(provider_address(name) as *const *const c_char) }
}

fn new_format() -> *mut c_void {
    let format = eclipse_amediaformat_new();
    assert!(!format.is_null(), "AMediaFormat_new returns a live format");
    format
}

fn delete(format: *mut c_void) -> MediaStatus {
    unsafe { eclipse_amediaformat_delete(format) }
}

fn set_int32(format: *mut c_void, key: &CStr, value: i32) {
    unsafe { eclipse_amediaformat_setint32(format, key.as_ptr(), value) }
}

fn set_float(format: *mut c_void, key: &CStr, value: f32) {
    unsafe { eclipse_amediaformat_setfloat(format, key.as_ptr(), value) }
}

fn set_string(format: *mut c_void, key: &CStr, value: &CStr) {
    unsafe { eclipse_amediaformat_setstring(format, key.as_ptr(), value.as_ptr()) }
}

fn set_buffer(format: *mut c_void, key: &CStr, bytes: &[u8]) {
    unsafe {
        eclipse_amediaformat_setbuffer(format, key.as_ptr(), bytes.as_ptr().cast(), bytes.len())
    }
}

fn get_int32(format: *mut c_void, key: &CStr) -> Option<i32> {
    let mut value = i32::MIN;
    unsafe { eclipse_amediaformat_getint32(format, key.as_ptr(), &mut value) }.then_some(value)
}

fn get_buffer(format: *mut c_void, key: &CStr) -> Option<(*mut c_void, usize)> {
    let mut data = std::ptr::null_mut();
    let mut size = usize::MAX;
    unsafe { eclipse_amediaformat_getbuffer(format, key.as_ptr(), &mut data, &mut size) }
        .then_some((data, size))
}

fn description(format: *mut c_void) -> String {
    let text = unsafe { eclipse_amediaformat_tostring(format) };
    assert!(!text.is_null(), "toString never returns NULL");
    unsafe { CStr::from_ptr(text) }
        .to_str()
        .expect("descriptions are UTF-8")
        .to_owned()
}

fn numbered_key(index: usize) -> CString {
    CString::new(format!("key-{index}")).expect("no NUL in a numbered key")
}

#[test]
fn values_round_trip_through_provider_natives_whether_keys_are_symbols_or_literals() {
    let new = unsafe { std::mem::transmute::<u64, NewFn>(provider_address("AMediaFormat_new")) };
    let delete =
        unsafe { std::mem::transmute::<u64, DeleteFn>(provider_address("AMediaFormat_delete")) };
    let set = unsafe {
        std::mem::transmute::<u64, SetInt32Fn>(provider_address("AMediaFormat_setInt32"))
    };
    let get = unsafe {
        std::mem::transmute::<u64, GetInt32Fn>(provider_address("AMediaFormat_getInt32"))
    };
    let read = |format, key: *const c_char| {
        let mut value = 0;
        unsafe { get(format, key, &mut value) }.then_some(value)
    };

    let format = unsafe { new() };
    assert!(!format.is_null());
    let width = key_symbol("AMEDIAFORMAT_KEY_WIDTH");
    let slice_height = CString::new("slice-height").expect("no NUL");

    unsafe {
        set(format, width, 64);
        set(format, c"slice-height".as_ptr(), 48);
    }
    assert_eq!(read(format, c"width".as_ptr()), Some(64));
    assert_eq!(read(format, slice_height.as_ptr()), Some(48));

    unsafe { set(format, c"width".as_ptr(), 1920) };
    assert_eq!(
        read(format, width),
        Some(1920),
        "a literal replaces a symbol's entry"
    );
    assert_eq!(read(format, key_symbol("AMEDIAFORMAT_KEY_HEIGHT")), None);
    assert_eq!(
        description(format),
        "width: int32(1920), slice-height: int32(48)"
    );

    assert_eq!(unsafe { delete(format) }, AMEDIA_OK);
}

#[test]
fn a_value_of_another_type_reads_as_missing_and_a_set_replaces_any_type() {
    let format = new_format();
    set_string(format, c"mime", c"video/avc");
    set_float(format, c"frame-rate", 30.0);

    let mut untouched = 7;
    let found = unsafe { eclipse_amediaformat_getint32(format, c"mime".as_ptr(), &mut untouched) };
    assert!(!found, "a string is not an int32");
    assert_eq!(untouched, 7, "a failed get leaves the output alone");
    assert_eq!(get_int32(format, c"frame-rate"), None);
    assert_eq!(get_buffer(format, c"mime"), None);
    assert_eq!(get_int32(format, c"missing"), None);

    set_int32(format, c"mime", 5);
    assert_eq!(get_int32(format, c"mime"), Some(5));
    assert_eq!(
        description(format),
        "mime: int32(5), frame-rate: float(30.000000)",
        "a replaced key keeps its place and takes the new type"
    );

    assert_eq!(delete(format), AMEDIA_OK);
}

#[test]
fn buffers_are_copied_in_and_come_back_intact_at_a_stable_address() {
    let format = new_format();
    let csd = [0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xc0, 0x1e, 0x00, 0xff];
    let mut source = csd;
    set_buffer(format, c"csd-0", &source);
    source.fill(0xaa);

    let (data, size) = get_buffer(format, c"csd-0").expect("csd-0 is a buffer");
    assert_eq!(size, csd.len());
    assert_eq!(
        unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size) },
        csd
    );

    for index in 0..32 {
        set_int32(format, &numbered_key(index), index as i32);
    }
    let (moved, moved_size) = get_buffer(format, c"csd-0").expect("csd-0 is still a buffer");
    assert_eq!(
        (moved, moved_size),
        (data, size),
        "growing the format keeps the buffer in place"
    );
    assert_eq!(
        unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size) },
        csd
    );

    unsafe { eclipse_amediaformat_setbuffer(format, c"csd-1".as_ptr(), std::ptr::null(), 0) };
    assert_eq!(get_buffer(format, c"csd-1").map(|(_, size)| size), Some(0));
    unsafe { eclipse_amediaformat_setbuffer(format, c"csd-2".as_ptr(), std::ptr::null(), 4) };
    assert_eq!(
        get_buffer(format, c"csd-2"),
        None,
        "NULL data with a size is dropped"
    );

    assert_eq!(delete(format), AMEDIA_OK);
}

#[test]
fn to_string_lists_each_key_with_its_type_and_caches_until_the_next_change() {
    let format = new_format();
    assert_eq!(description(format), "");

    set_string(format, c"mime", c"video/avc");
    set_int32(format, c"width", 64);
    set_float(format, c"frame-rate", 30.0);
    set_buffer(format, c"csd-0", b"codec-private-bytes");

    let first = unsafe { eclipse_amediaformat_tostring(format) };
    let expected = "mime: string(video/avc), width: int32(64), frame-rate: float(30.000000), \
                    csd-0: data(19 bytes)";
    assert_eq!(unsafe { CStr::from_ptr(first) }.to_str(), Ok(expected));
    assert_eq!(
        unsafe { eclipse_amediaformat_tostring(format) },
        first,
        "an unchanged format returns its cached string"
    );

    set_int32(format, c"width", 1280);
    assert_eq!(
        unsafe { CStr::from_ptr(first) }.to_str(),
        Ok(expected),
        "the previous string stays readable until toString runs again"
    );
    assert_eq!(
        description(format),
        "mime: string(video/avc), width: int32(1280), frame-rate: float(30.000000), \
         csd-0: data(19 bytes)"
    );

    assert_eq!(delete(format), AMEDIA_OK);
}

#[test]
fn a_format_holds_at_most_64_keys_and_drops_the_next_new_key_with_a_warning() {
    let format = new_format();
    for index in 0..MAX_FORMAT_ENTRIES {
        set_int32(format, &numbered_key(index), index as i32);
    }

    let log = formatted_log("mediandk=warn", || set_int32(format, c"one-too-many", 1));
    assert_eq!(get_int32(format, c"one-too-many"), None);
    let line = log
        .lines()
        .find(|line| line.contains("AMediaFormat_setInt32 dropped its value"))
        .unwrap_or_else(|| panic!("no dropped-value warning in {log:?}"));
    assert!(line.contains("WARN mediandk:"), "{line}");
    assert!(line.contains("one-too-many"), "{line}");
    assert!(line.contains("already holds 64 keys"), "{line}");

    set_int32(format, &numbered_key(0), -1);
    assert_eq!(
        get_int32(format, &numbered_key(0)),
        Some(-1),
        "a full format still replaces existing keys"
    );
    assert_eq!(description(format).split(", ").count(), MAX_FORMAT_ENTRIES);

    assert_eq!(delete(format), AMEDIA_OK);
}

#[test]
fn deleted_stale_and_fabricated_formats_are_invalid_objects() {
    let format = new_format();
    set_int32(format, c"width", 64);
    set_buffer(format, c"csd-0", b"\x01\x02");
    let next_generation = (format as usize).wrapping_add(1 << 32) as *mut c_void;
    assert_eq!(delete(next_generation), AMEDIA_ERROR_INVALID_OBJECT);
    assert_eq!(get_int32(format, c"width"), Some(64));

    assert_eq!(delete(format), AMEDIA_OK);
    assert_eq!(delete(format), AMEDIA_ERROR_INVALID_OBJECT);
    assert_eq!(AMEDIA_ERROR_INVALID_OBJECT, -10003);
    assert_eq!(get_int32(format, c"width"), None);
    assert_eq!(get_buffer(format, c"csd-0"), None);
    set_int32(format, c"width", 1);
    assert_eq!(
        get_int32(format, c"width"),
        None,
        "a dead format stores nothing"
    );
    assert_eq!(description(format), "");

    for fabricated in [0, 0xdead_beef, usize::MAX] {
        let fabricated = fabricated as *mut c_void;
        assert_eq!(delete(fabricated), AMEDIA_ERROR_INVALID_OBJECT);
        assert_eq!(get_int32(fabricated, c"width"), None);
        assert_eq!(description(fabricated), "");
    }
}

#[test]
fn null_keys_values_and_outputs_return_false_and_store_nothing() {
    let format = new_format();
    unsafe {
        eclipse_amediaformat_setint32(format, std::ptr::null(), 1);
        eclipse_amediaformat_setstring(format, c"mime".as_ptr(), std::ptr::null());
    }
    assert_eq!(description(format), "");

    set_int32(format, c"width", 64);
    set_buffer(format, c"csd-0", b"\x01");
    let mut value = 0;
    let mut data = std::ptr::null_mut();
    let mut size = 0;
    unsafe {
        assert!(!eclipse_amediaformat_getint32(
            format,
            std::ptr::null(),
            &mut value
        ));
        assert!(!eclipse_amediaformat_getint32(
            format,
            c"width".as_ptr(),
            std::ptr::null_mut()
        ));
        assert!(!eclipse_amediaformat_getbuffer(
            format,
            std::ptr::null(),
            &mut data,
            &mut size
        ));
        assert!(!eclipse_amediaformat_getbuffer(
            format,
            c"csd-0".as_ptr(),
            std::ptr::null_mut(),
            &mut size
        ));
    }
    assert_eq!(
        description(format),
        "width: int32(64), csd-0: data(1 bytes)"
    );

    assert_eq!(delete(format), AMEDIA_OK);
}
