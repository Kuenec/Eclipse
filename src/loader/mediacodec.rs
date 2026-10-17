use std::ffi::{c_char, c_int, CStr};
use std::fmt;
use std::sync::OnceLock;

pub(crate) mod avcodec;
mod codec;
mod fixture;
mod format;
mod image_reader;
pub mod self_test;

pub const MEDIA_NDK_NATIVE_COUNT: usize = 54;

type MediaStatus = c_int;

const AMEDIA_OK: MediaStatus = 0;
const AMEDIA_ERROR_BASE: MediaStatus = -10000;
const AMEDIA_ERROR_UNKNOWN: MediaStatus = AMEDIA_ERROR_BASE;
const AMEDIA_ERROR_UNSUPPORTED: MediaStatus = AMEDIA_ERROR_BASE - 2;
const AMEDIA_ERROR_INVALID_OBJECT: MediaStatus = AMEDIA_ERROR_BASE - 3;
const AMEDIA_ERROR_INVALID_PARAMETER: MediaStatus = AMEDIA_ERROR_BASE - 4;
const AMEDIA_ERROR_INVALID_OPERATION: MediaStatus = AMEDIA_ERROR_BASE - 5;

const KEY_BIT_RATE: &CStr = c"bitrate";
const KEY_CHANNEL_COUNT: &CStr = c"channel-count";
const KEY_COLOR_FORMAT: &CStr = c"color-format";
const KEY_FRAME_RATE: &CStr = c"frame-rate";
const KEY_HEIGHT: &CStr = c"height";
const KEY_I_FRAME_INTERVAL: &CStr = c"i-frame-interval";
const KEY_MIME: &CStr = c"mime";
const KEY_SAMPLE_RATE: &CStr = c"sample-rate";
const KEY_STRIDE: &CStr = c"stride";
const KEY_WIDTH: &CStr = c"width";

unsafe fn c_text<'a>(text: *const c_char) -> Option<&'a CStr> {
    if text.is_null() {
        return None;
    }
    Some(unsafe { CStr::from_ptr(text) })
}

static AMEDIAFORMAT_KEYS: [(&str, &CStr); 10] = [
    ("AMEDIAFORMAT_KEY_BIT_RATE", KEY_BIT_RATE),
    ("AMEDIAFORMAT_KEY_CHANNEL_COUNT", KEY_CHANNEL_COUNT),
    ("AMEDIAFORMAT_KEY_COLOR_FORMAT", KEY_COLOR_FORMAT),
    ("AMEDIAFORMAT_KEY_FRAME_RATE", KEY_FRAME_RATE),
    ("AMEDIAFORMAT_KEY_HEIGHT", KEY_HEIGHT),
    ("AMEDIAFORMAT_KEY_I_FRAME_INTERVAL", KEY_I_FRAME_INTERVAL),
    ("AMEDIAFORMAT_KEY_MIME", KEY_MIME),
    ("AMEDIAFORMAT_KEY_SAMPLE_RATE", KEY_SAMPLE_RATE),
    ("AMEDIAFORMAT_KEY_STRIDE", KEY_STRIDE),
    ("AMEDIAFORMAT_KEY_WIDTH", KEY_WIDTH),
];

struct KeyPtrTable([*const c_char; 10]);

unsafe impl Sync for KeyPtrTable {}

unsafe impl Send for KeyPtrTable {}

static AMEDIAFORMAT_KEY_PTRS: OnceLock<KeyPtrTable> = OnceLock::new();

fn amediaformat_key_addr(idx: usize) -> u64 {
    let t = AMEDIAFORMAT_KEY_PTRS
        .get_or_init(|| KeyPtrTable(AMEDIAFORMAT_KEYS.map(|(_, key)| key.as_ptr())));
    std::ptr::addr_of!(t.0[idx]) as u64
}

pub(crate) fn register_natives(mut register: impl FnMut(&'static str, u64)) {
    macro_rules! reg {
        ($name:literal, $f:expr) => {
            register($name, $f as *const () as u64);
        };
    }

    reg!(
        "AMediaCodec_configure",
        codec::eclipse_amediacodec_configure
    );
    reg!(
        "AMediaCodec_createDecoderByType",
        codec::eclipse_amediacodec_createdecoderbytype
    );
    reg!(
        "AMediaCodec_createEncoderByType",
        codec::eclipse_amediacodec_createencoderbytype
    );
    reg!(
        "AMediaCodec_createInputSurface",
        codec::eclipse_amediacodec_createinputsurface
    );
    reg!("AMediaCodec_delete", codec::eclipse_amediacodec_delete);
    reg!(
        "AMediaCodec_dequeueInputBuffer",
        codec::eclipse_amediacodec_dequeueinputbuffer
    );
    reg!(
        "AMediaCodec_dequeueOutputBuffer",
        codec::eclipse_amediacodec_dequeueoutputbuffer
    );
    reg!("AMediaCodec_flush", codec::eclipse_amediacodec_flush);
    reg!(
        "AMediaCodec_getInputBuffer",
        codec::eclipse_amediacodec_getinputbuffer
    );
    reg!(
        "AMediaCodec_getOutputBuffer",
        codec::eclipse_amediacodec_getoutputbuffer
    );
    reg!(
        "AMediaCodec_getOutputFormat",
        codec::eclipse_amediacodec_getoutputformat
    );
    reg!(
        "AMediaCodec_queueInputBuffer",
        codec::eclipse_amediacodec_queueinputbuffer
    );
    reg!(
        "AMediaCodec_releaseOutputBuffer",
        codec::eclipse_amediacodec_releaseoutputbuffer
    );
    reg!(
        "AMediaCodec_signalEndOfInputStream",
        codec::eclipse_amediacodec_signalendofinputstream
    );
    reg!("AMediaCodec_start", codec::eclipse_amediacodec_start);
    reg!("AMediaCodec_stop", codec::eclipse_amediacodec_stop);

    reg!("AMediaFormat_delete", format::eclipse_amediaformat_delete);
    reg!(
        "AMediaFormat_getBuffer",
        format::eclipse_amediaformat_getbuffer
    );
    reg!(
        "AMediaFormat_getInt32",
        format::eclipse_amediaformat_getint32
    );
    reg!("AMediaFormat_new", format::eclipse_amediaformat_new);
    reg!(
        "AMediaFormat_setBuffer",
        format::eclipse_amediaformat_setbuffer
    );
    reg!(
        "AMediaFormat_setFloat",
        format::eclipse_amediaformat_setfloat
    );
    reg!(
        "AMediaFormat_setInt32",
        format::eclipse_amediaformat_setint32
    );
    reg!(
        "AMediaFormat_setString",
        format::eclipse_amediaformat_setstring
    );
    reg!(
        "AMediaFormat_toString",
        format::eclipse_amediaformat_tostring
    );

    for (index, (name, _)) in AMEDIAFORMAT_KEYS.iter().enumerate() {
        register(name, amediaformat_key_addr(index));
    }

    reg!("AImageReader_new", image_reader::eclipse_aimagereader_new);
    reg!(
        "AImageReader_newWithUsage",
        image_reader::eclipse_aimagereader_newwithusage
    );
    reg!(
        "AImageReader_getWindow",
        image_reader::eclipse_aimagereader_getwindow
    );
    reg!(
        "AImageReader_acquireLatestImage",
        image_reader::eclipse_aimagereader_acquirelatestimage
    );
    reg!(
        "AImageReader_setImageListener",
        image_reader::eclipse_aimagereader_setimagelistener
    );
    reg!(
        "AImageReader_setBufferRemovedListener",
        image_reader::eclipse_aimagereader_setbufferremovedlistener
    );
    reg!(
        "AImageReader_delete",
        image_reader::eclipse_aimagereader_delete
    );

    reg!(
        "AImage_getPlaneData",
        image_reader::eclipse_aimage_getplanedata
    );
    reg!(
        "AImage_getPlaneRowStride",
        image_reader::eclipse_aimage_getplanerowstride
    );
    reg!(
        "AImage_getPlanePixelStride",
        image_reader::eclipse_aimage_getplanepixelstride
    );
    reg!(
        "AImage_getCropRect",
        image_reader::eclipse_aimage_getcroprect
    );
    reg!(
        "AImage_getTimestamp",
        image_reader::eclipse_aimage_gettimestamp
    );
    reg!("AImage_getFormat", image_reader::eclipse_aimage_getformat);
    reg!(
        "AImage_getHardwareBuffer",
        image_reader::eclipse_aimage_gethardwarebuffer
    );
    reg!("AImage_delete", image_reader::eclipse_aimage_delete);
    reg!(
        "AImage_deleteAsync",
        image_reader::eclipse_aimage_deleteasync
    );

    reg!(
        "AHardwareBuffer_acquire",
        image_reader::eclipse_ahardwarebuffer_acquire
    );
    reg!(
        "AHardwareBuffer_describe",
        image_reader::eclipse_ahardwarebuffer_describe
    );
    reg!(
        "AHardwareBuffer_release",
        image_reader::eclipse_ahardwarebuffer_release
    );
}

pub struct Diagnosis(avcodec::Backend);

#[must_use]
pub fn diagnose() -> Diagnosis {
    Diagnosis(avcodec::Backend::load(&avcodec::RUNTIME_LIBRARIES))
}

impl Diagnosis {
    pub fn problem(&self) -> Option<String> {
        video_problem(self.0.library().map(|_| self.0.decoders()))
    }
}

impl fmt::Display for Diagnosis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

fn video_problem(
    decoders: Result<&[avcodec::Codec], &avcodec::BackendUnavailable>,
) -> Option<String> {
    match decoders {
        Err(reason) => Some(format!("{reason}.")),
        Ok(decoders) if !avcodec::missing_codecs_extra(decoders).is_empty() => Some(format!(
            "H.264 and HEVC videos in games will not play: {}.",
            avcodec::CODECS_EXTRA_ADVICE
        )),
        Ok(_) => None,
    }
}

#[cfg(test)]
mod tests;
