use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::OnceLock;

pub const MEDIA_NDK_NATIVE_COUNT: usize = 33;

type MediaStatus = c_int;

const AMEDIA_ERROR_BASE: MediaStatus = -10000;

const AMEDIA_ERROR_UNSUPPORTED: MediaStatus = AMEDIA_ERROR_BASE - 2;

unsafe fn c_text<'a>(text: *const c_char) -> Option<&'a CStr> {
    if text.is_null() {
        return None;
    }
    Some(unsafe { CStr::from_ptr(text) })
}

unsafe extern "C" fn eclipse_amediacodec_configure(
    codec: *mut c_void,
    format: *const c_void,
    surface: *mut c_void,
    crypto: *mut c_void,
    flags: u32,
) -> MediaStatus {
    let status = AMEDIA_ERROR_UNSUPPORTED;
    tracing::debug!(
        target: "mediandk",
        ?codec,
        ?format,
        ?surface,
        ?crypto,
        flags,
        status,
        "AMediaCodec_configure"
    );
    status
}

unsafe extern "C" fn eclipse_amediacodec_createdecoderbytype(
    mime_type: *const c_char,
) -> *mut c_void {
    let codec: *mut c_void = std::ptr::null_mut();
    tracing::debug!(
        target: "mediandk",
        mime = ?unsafe { c_text(mime_type) },
        ?codec,
        "AMediaCodec_createDecoderByType"
    );
    codec
}

unsafe extern "C" fn eclipse_amediacodec_createencoderbytype(
    mime_type: *const c_char,
) -> *mut c_void {
    let codec: *mut c_void = std::ptr::null_mut();
    tracing::debug!(
        target: "mediandk",
        mime = ?unsafe { c_text(mime_type) },
        ?codec,
        "AMediaCodec_createEncoderByType"
    );
    codec
}

unsafe extern "C" fn eclipse_amediacodec_delete(codec: *mut c_void) -> MediaStatus {
    let status = AMEDIA_ERROR_UNSUPPORTED;
    tracing::debug!(target: "mediandk", ?codec, status, "AMediaCodec_delete");
    status
}

unsafe extern "C" fn eclipse_amediacodec_dequeueinputbuffer(
    codec: *mut c_void,
    timeout_us: i64,
) -> isize {
    let index = AMEDIA_ERROR_UNSUPPORTED as isize;
    tracing::trace!(
        target: "mediandk",
        ?codec,
        timeout_us,
        index,
        "AMediaCodec_dequeueInputBuffer"
    );
    index
}

unsafe extern "C" fn eclipse_amediacodec_dequeueoutputbuffer(
    codec: *mut c_void,
    _info: *mut c_void,
    timeout_us: i64,
) -> isize {
    let index = AMEDIA_ERROR_UNSUPPORTED as isize;
    tracing::trace!(
        target: "mediandk",
        ?codec,
        timeout_us,
        index,
        "AMediaCodec_dequeueOutputBuffer"
    );
    index
}

unsafe extern "C" fn eclipse_amediacodec_flush(codec: *mut c_void) -> MediaStatus {
    let status = AMEDIA_ERROR_UNSUPPORTED;
    tracing::debug!(target: "mediandk", ?codec, status, "AMediaCodec_flush");
    status
}

unsafe extern "C" fn eclipse_amediacodec_getinputbuffer(
    codec: *mut c_void,
    index: usize,
    _out_size: *mut usize,
) -> *mut u8 {
    let buffer: *mut u8 = std::ptr::null_mut();
    tracing::trace!(
        target: "mediandk",
        ?codec,
        index,
        ?buffer,
        "AMediaCodec_getInputBuffer"
    );
    buffer
}

unsafe extern "C" fn eclipse_amediacodec_getoutputbuffer(
    codec: *mut c_void,
    index: usize,
    _out_size: *mut usize,
) -> *mut u8 {
    let buffer: *mut u8 = std::ptr::null_mut();
    tracing::trace!(
        target: "mediandk",
        ?codec,
        index,
        ?buffer,
        "AMediaCodec_getOutputBuffer"
    );
    buffer
}

unsafe extern "C" fn eclipse_amediacodec_getoutputformat(codec: *mut c_void) -> *mut c_void {
    let format: *mut c_void = std::ptr::null_mut();
    tracing::debug!(target: "mediandk", ?codec, ?format, "AMediaCodec_getOutputFormat");
    format
}

unsafe extern "C" fn eclipse_amediacodec_queueinputbuffer(
    codec: *mut c_void,
    index: usize,
    offset: libc::off_t,
    size: usize,
    pts_us: u64,
    flags: u32,
) -> MediaStatus {
    let status = AMEDIA_ERROR_UNSUPPORTED;
    tracing::trace!(
        target: "mediandk",
        ?codec,
        index,
        offset,
        size,
        pts_us,
        flags,
        status,
        "AMediaCodec_queueInputBuffer"
    );
    status
}

unsafe extern "C" fn eclipse_amediacodec_releaseoutputbuffer(
    codec: *mut c_void,
    index: usize,
    render: bool,
) -> MediaStatus {
    let status = AMEDIA_ERROR_UNSUPPORTED;
    tracing::trace!(
        target: "mediandk",
        ?codec,
        index,
        render,
        status,
        "AMediaCodec_releaseOutputBuffer"
    );
    status
}

unsafe extern "C" fn eclipse_amediacodec_start(codec: *mut c_void) -> MediaStatus {
    let status = AMEDIA_ERROR_UNSUPPORTED;
    tracing::debug!(target: "mediandk", ?codec, status, "AMediaCodec_start");
    status
}

unsafe extern "C" fn eclipse_amediacodec_stop(codec: *mut c_void) -> MediaStatus {
    let status = AMEDIA_ERROR_UNSUPPORTED;
    tracing::debug!(target: "mediandk", ?codec, status, "AMediaCodec_stop");
    status
}

extern "C" fn eclipse_amediaformat_new() -> *mut c_void {
    let format: *mut c_void = std::ptr::null_mut();
    tracing::debug!(target: "mediandk", ?format, "AMediaFormat_new");
    format
}

unsafe extern "C" fn eclipse_amediaformat_delete(format: *mut c_void) -> MediaStatus {
    let status = AMEDIA_ERROR_UNSUPPORTED;
    tracing::debug!(target: "mediandk", ?format, status, "AMediaFormat_delete");
    status
}

unsafe extern "C" fn eclipse_amediaformat_getint32(
    format: *mut c_void,
    name: *const c_char,
    _out: *mut i32,
) -> bool {
    let found = false;
    tracing::debug!(
        target: "mediandk",
        ?format,
        key = ?unsafe { c_text(name) },
        found,
        "AMediaFormat_getInt32"
    );
    found
}

unsafe extern "C" fn eclipse_amediaformat_getbuffer(
    format: *mut c_void,
    name: *const c_char,
    _data: *mut *mut c_void,
    _size: *mut usize,
) -> bool {
    let found = false;
    tracing::debug!(
        target: "mediandk",
        ?format,
        key = ?unsafe { c_text(name) },
        found,
        "AMediaFormat_getBuffer"
    );
    found
}

unsafe extern "C" fn eclipse_amediaformat_setint32(
    format: *mut c_void,
    name: *const c_char,
    value: i32,
) {
    tracing::debug!(
        target: "mediandk",
        ?format,
        key = ?unsafe { c_text(name) },
        value,
        "AMediaFormat_setInt32"
    );
}

unsafe extern "C" fn eclipse_amediaformat_setfloat(
    format: *mut c_void,
    name: *const c_char,
    value: f32,
) {
    tracing::debug!(
        target: "mediandk",
        ?format,
        key = ?unsafe { c_text(name) },
        value,
        "AMediaFormat_setFloat"
    );
}

unsafe extern "C" fn eclipse_amediaformat_setstring(
    format: *mut c_void,
    name: *const c_char,
    value: *const c_char,
) {
    tracing::debug!(
        target: "mediandk",
        ?format,
        key = ?unsafe { c_text(name) },
        value = ?unsafe { c_text(value) },
        "AMediaFormat_setString"
    );
}

unsafe extern "C" fn eclipse_amediaformat_setbuffer(
    format: *mut c_void,
    name: *const c_char,
    _data: *const c_void,
    size: usize,
) {
    tracing::debug!(
        target: "mediandk",
        ?format,
        key = ?unsafe { c_text(name) },
        size,
        "AMediaFormat_setBuffer"
    );
}

static EMPTY_CSTR: [u8; 1] = [0];

unsafe extern "C" fn eclipse_amediaformat_tostring(format: *mut c_void) -> *const c_char {
    tracing::debug!(target: "mediandk", ?format, "AMediaFormat_toString");
    EMPTY_CSTR.as_ptr() as *const c_char
}

static AMEDIAFORMAT_KEY_STRINGS: [&[u8]; 10] = [
    b"bitrate\0",
    b"channel-count\0",
    b"color-format\0",
    b"frame-rate\0",
    b"height\0",
    b"i-frame-interval\0",
    b"mime\0",
    b"sample-rate\0",
    b"stride\0",
    b"width\0",
];

struct KeyPtrTable([*const c_char; 10]);

unsafe impl Sync for KeyPtrTable {}

unsafe impl Send for KeyPtrTable {}

static AMEDIAFORMAT_KEY_PTRS: OnceLock<KeyPtrTable> = OnceLock::new();

fn amediaformat_key_addr(idx: usize) -> u64 {
    let t = AMEDIAFORMAT_KEY_PTRS.get_or_init(|| {
        let mut ptrs = [std::ptr::null::<c_char>(); 10];
        for (slot, s) in ptrs.iter_mut().zip(AMEDIAFORMAT_KEY_STRINGS.iter()) {
            *slot = s.as_ptr() as *const c_char;
        }
        KeyPtrTable(ptrs)
    });
    std::ptr::addr_of!(t.0[idx]) as u64
}

pub(crate) fn register_natives(mut register: impl FnMut(&'static str, u64)) {
    macro_rules! reg {
        ($name:literal, $f:expr) => {
            register($name, $f as *const () as u64);
        };
    }

    reg!("AMediaCodec_configure", eclipse_amediacodec_configure);
    reg!(
        "AMediaCodec_createDecoderByType",
        eclipse_amediacodec_createdecoderbytype
    );
    reg!(
        "AMediaCodec_createEncoderByType",
        eclipse_amediacodec_createencoderbytype
    );
    reg!("AMediaCodec_delete", eclipse_amediacodec_delete);
    reg!(
        "AMediaCodec_dequeueInputBuffer",
        eclipse_amediacodec_dequeueinputbuffer
    );
    reg!(
        "AMediaCodec_dequeueOutputBuffer",
        eclipse_amediacodec_dequeueoutputbuffer
    );
    reg!("AMediaCodec_flush", eclipse_amediacodec_flush);
    reg!(
        "AMediaCodec_getInputBuffer",
        eclipse_amediacodec_getinputbuffer
    );
    reg!(
        "AMediaCodec_getOutputBuffer",
        eclipse_amediacodec_getoutputbuffer
    );
    reg!(
        "AMediaCodec_getOutputFormat",
        eclipse_amediacodec_getoutputformat
    );
    reg!(
        "AMediaCodec_queueInputBuffer",
        eclipse_amediacodec_queueinputbuffer
    );
    reg!(
        "AMediaCodec_releaseOutputBuffer",
        eclipse_amediacodec_releaseoutputbuffer
    );
    reg!("AMediaCodec_start", eclipse_amediacodec_start);
    reg!("AMediaCodec_stop", eclipse_amediacodec_stop);

    reg!("AMediaFormat_delete", eclipse_amediaformat_delete);
    reg!("AMediaFormat_getBuffer", eclipse_amediaformat_getbuffer);
    reg!("AMediaFormat_getInt32", eclipse_amediaformat_getint32);
    reg!("AMediaFormat_new", eclipse_amediaformat_new);
    reg!("AMediaFormat_setBuffer", eclipse_amediaformat_setbuffer);
    reg!("AMediaFormat_setFloat", eclipse_amediaformat_setfloat);
    reg!("AMediaFormat_setInt32", eclipse_amediaformat_setint32);
    reg!("AMediaFormat_setString", eclipse_amediaformat_setstring);
    reg!("AMediaFormat_toString", eclipse_amediaformat_tostring);

    register("AMEDIAFORMAT_KEY_BIT_RATE", amediaformat_key_addr(0));
    register("AMEDIAFORMAT_KEY_CHANNEL_COUNT", amediaformat_key_addr(1));
    register("AMEDIAFORMAT_KEY_COLOR_FORMAT", amediaformat_key_addr(2));
    register("AMEDIAFORMAT_KEY_FRAME_RATE", amediaformat_key_addr(3));
    register("AMEDIAFORMAT_KEY_HEIGHT", amediaformat_key_addr(4));
    register(
        "AMEDIAFORMAT_KEY_I_FRAME_INTERVAL",
        amediaformat_key_addr(5),
    );
    register("AMEDIAFORMAT_KEY_MIME", amediaformat_key_addr(6));
    register("AMEDIAFORMAT_KEY_SAMPLE_RATE", amediaformat_key_addr(7));
    register("AMEDIAFORMAT_KEY_STRIDE", amediaformat_key_addr(8));
    register("AMEDIAFORMAT_KEY_WIDTH", amediaformat_key_addr(9));
}

#[cfg(test)]
mod tests;
