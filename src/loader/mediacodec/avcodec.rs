use std::ffi::{c_char, c_int, c_uint, c_void, CStr};
use std::fmt;
use std::marker::PhantomData;
use std::mem::offset_of;
use std::num::NonZeroUsize;
use std::ptr::{self, NonNull};
use std::sync::OnceLock;

use libloading::os::unix::{Library, RTLD_LOCAL, RTLD_NOW};

const fn ffmpeg_error_tag(tag: [u8; 4]) -> c_int {
    -i32::from_le_bytes(tag)
}

const AVERROR_EAGAIN: c_int = -libc::EAGAIN;
const AVERROR_EOF: c_int = ffmpeg_error_tag(*b"EOF ");
const AVERROR_INVALIDDATA: c_int = ffmpeg_error_tag(*b"INDA");

const AV_PKT_FLAG_KEY: c_int = 1;
const AV_NOPTS_VALUE: i64 = i64::MIN;
const AV_INPUT_BUFFER_PADDING_SIZE: usize = 64;
const AV_ERROR_MAX_STRING_SIZE: usize = 64;

const MAX_VIDEO_DECODER_THREADS: usize = 4;

pub(crate) const CODECS_EXTRA_ADVICE: &str =
    "H.264/HEVC need org.freedesktop.Platform.codecs-extra//26.08-extra; \
     install it with flatpak install flathub org.freedesktop.Platform.codecs-extra//26.08-extra";

#[derive(Clone, Copy, Debug)]
pub(crate) struct LibraryPair {
    avcodec: &'static str,
    avcodec_major: u32,
    avutil: &'static str,
    avutil_major: u32,
}

pub(crate) const RUNTIME_LIBRARIES: [LibraryPair; 2] = [
    LibraryPair {
        avcodec: "libavcodec.so.62",
        avcodec_major: 62,
        avutil: "libavutil.so.60",
        avutil_major: 60,
    },
    LibraryPair {
        avcodec: "libavcodec.so.63",
        avcodec_major: 63,
        avutil: "libavutil.so.61",
        avutil_major: 61,
    },
];

#[repr(C)]
struct AvCodec {
    _opaque: [u8; 0],
}

#[repr(C)]
struct AvCodecContext {
    _opaque: [u8; 0],
}

#[repr(C)]
struct AvFrame {
    data: [*mut u8; 8],
    linesize: [c_int; 8],
    extended_data: *mut *mut u8,
    width: c_int,
    height: c_int,
    nb_samples: c_int,
    format: c_int,
    unread_after_format: [u8; 16],
    pts: i64,
    unread_after_pts: [u8; 36],
    sample_rate: c_int,
    unread_after_sample_rate: [u8; 200],
    ch_layout_order: c_int,
    ch_layout_nb_channels: c_int,
}

#[repr(C)]
struct AvPacket {
    buf: *mut c_void,
    pts: i64,
    dts: i64,
    data: *mut u8,
    size: c_int,
    stream_index: c_int,
    flags: c_int,
}

#[repr(C)]
struct AvCodecParameters {
    codec_type: c_int,
    codec_id: c_int,
    codec_tag: c_uint,
    extradata: *mut u8,
    extradata_size: c_int,
}

const _: () = {
    assert!(offset_of!(AvFrame, data) == 0);
    assert!(offset_of!(AvFrame, linesize) == 64);
    assert!(offset_of!(AvFrame, extended_data) == 96);
    assert!(offset_of!(AvFrame, width) == 104);
    assert!(offset_of!(AvFrame, height) == 108);
    assert!(offset_of!(AvFrame, nb_samples) == 112);
    assert!(offset_of!(AvFrame, format) == 116);
    assert!(offset_of!(AvFrame, pts) == 136);
    assert!(offset_of!(AvFrame, sample_rate) == 180);
    assert!(offset_of!(AvFrame, ch_layout_order) == 384);
    assert!(offset_of!(AvFrame, ch_layout_nb_channels) == 388);
    assert!(offset_of!(AvPacket, pts) == 8);
    assert!(offset_of!(AvPacket, dts) == 16);
    assert!(offset_of!(AvPacket, data) == 24);
    assert!(offset_of!(AvPacket, size) == 32);
    assert!(offset_of!(AvPacket, flags) == 40);
    assert!(offset_of!(AvCodecParameters, extradata) == 16);
    assert!(offset_of!(AvCodecParameters, extradata_size) == 24);
};

#[derive(Clone, Copy)]
struct Functions {
    avcodec_find_decoder_by_name: unsafe extern "C" fn(*const c_char) -> *const AvCodec,
    avcodec_alloc_context3: unsafe extern "C" fn(*const AvCodec) -> *mut AvCodecContext,
    avcodec_free_context: unsafe extern "C" fn(*mut *mut AvCodecContext),
    avcodec_parameters_alloc: unsafe extern "C" fn() -> *mut AvCodecParameters,
    avcodec_parameters_free: unsafe extern "C" fn(*mut *mut AvCodecParameters),
    avcodec_parameters_from_context:
        unsafe extern "C" fn(*mut AvCodecParameters, *const AvCodecContext) -> c_int,
    avcodec_parameters_to_context:
        unsafe extern "C" fn(*mut AvCodecContext, *const AvCodecParameters) -> c_int,
    avcodec_open2:
        unsafe extern "C" fn(*mut AvCodecContext, *const AvCodec, *mut *mut c_void) -> c_int,
    avcodec_send_packet: unsafe extern "C" fn(*mut AvCodecContext, *const AvPacket) -> c_int,
    avcodec_receive_frame: unsafe extern "C" fn(*mut AvCodecContext, *mut AvFrame) -> c_int,
    avcodec_flush_buffers: unsafe extern "C" fn(*mut AvCodecContext),
    av_packet_alloc: unsafe extern "C" fn() -> *mut AvPacket,
    av_packet_free: unsafe extern "C" fn(*mut *mut AvPacket),
    av_new_packet: unsafe extern "C" fn(*mut AvPacket, c_int) -> c_int,
    av_packet_unref: unsafe extern "C" fn(*mut AvPacket),
    av_frame_alloc: unsafe extern "C" fn() -> *mut AvFrame,
    av_frame_free: unsafe extern "C" fn(*mut *mut AvFrame),
    av_mallocz: unsafe extern "C" fn(usize) -> *mut c_void,
    av_opt_set_int: unsafe extern "C" fn(*mut c_void, *const c_char, i64, c_int) -> c_int,
    av_get_pix_fmt: unsafe extern "C" fn(*const c_char) -> c_int,
    av_get_pix_fmt_name: unsafe extern "C" fn(c_int) -> *const c_char,
    av_get_sample_fmt: unsafe extern "C" fn(*const c_char) -> c_int,
    av_get_sample_fmt_name: unsafe extern "C" fn(c_int) -> *const c_char,
    av_strerror: unsafe extern "C" fn(c_int, *mut c_char, usize) -> c_int,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LibraryVersion(u32);

impl LibraryVersion {
    pub(crate) fn major(self) -> u32 {
        self.0 >> 16
    }
}

impl fmt::Display for LibraryVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{}.{}",
            self.major(),
            (self.0 >> 8) & 0xff,
            self.0 & 0xff
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BackendUnavailable {
    NotFound {
        tried: Vec<&'static str>,
    },
    VersionMismatch {
        soname: &'static str,
        expected: u32,
        found: LibraryVersion,
    },
    MissingSymbol {
        name: &'static str,
    },
    UnknownFormat {
        name: &'static CStr,
    },
}

impl fmt::Display for BackendUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { tried } => write!(
                f,
                "FFmpeg is not installed (tried {}); run Eclipse from its Flatpak, whose runtime \
                 provides FFmpeg, or install FFmpeg 8 or 9",
                tried.join(", ")
            )?,
            Self::VersionMismatch {
                soname,
                expected,
                found,
            } => write!(
                f,
                "{soname} reports version {found} instead of major version {expected}, so \
                 Eclipse does not use it"
            )?,
            Self::MissingSymbol { name } => write!(f, "FFmpeg has no {name} function")?,
            Self::UnknownFormat { name } => write!(
                f,
                "FFmpeg does not know the {} format",
                name.to_string_lossy()
            )?,
        }
        f.write_str("; H.264, HEVC and AAC media will not play")
    }
}

impl std::error::Error for BackendUnavailable {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Codec {
    Aac,
    H264,
    Hevc,
    Mp3,
    Opus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Media {
    Audio,
    Video,
}

impl Codec {
    pub(crate) const ALL: [Codec; 5] = [
        Codec::Aac,
        Codec::H264,
        Codec::Hevc,
        Codec::Mp3,
        Codec::Opus,
    ];

    pub(crate) fn for_mime(mime: &[u8]) -> Option<Codec> {
        Codec::ALL
            .into_iter()
            .find(|codec| mime.eq_ignore_ascii_case(codec.mime().as_bytes()))
    }

    pub(crate) fn mime(self) -> &'static str {
        match self {
            Codec::Aac => "audio/mp4a-latm",
            Codec::H264 => "video/avc",
            Codec::Hevc => "video/hevc",
            Codec::Mp3 => "audio/mpeg",
            Codec::Opus => "audio/opus",
        }
    }

    pub(crate) fn decoder_name(self) -> &'static CStr {
        match self {
            Codec::Aac => c"aac",
            Codec::H264 => c"h264",
            Codec::Hevc => c"hevc",
            Codec::Mp3 => c"mp3",
            Codec::Opus => c"opus",
        }
    }

    pub(crate) fn needs_codecs_extra(self) -> bool {
        matches!(self, Codec::H264 | Codec::Hevc)
    }

    pub(crate) fn media(self) -> Media {
        match self {
            Codec::Aac | Codec::Mp3 | Codec::Opus => Media::Audio,
            Codec::H264 | Codec::Hevc => Media::Video,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PixelFormat {
    Yuv420p,
    Yuvj420p,
    Nv12,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SampleFormat {
    S16,
    S16Planar,
    F32,
    F32Planar,
}

impl SampleFormat {
    fn planar(self) -> bool {
        matches!(self, SampleFormat::S16Planar | SampleFormat::F32Planar)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PcmEncoding {
    I16,
    F32,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DecodeError {
    DecoderMissing(Codec),
    OutOfMemory(&'static str),
    TooLarge {
        what: &'static str,
        len: usize,
    },
    InvalidData,
    Ffmpeg {
        operation: &'static str,
        code: c_int,
        reason: String,
    },
    UnsupportedPixelFormat(String),
    UnsupportedSampleFormat(String),
    MalformedFrame(&'static str),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DecoderMissing(codec) => write!(
                f,
                "FFmpeg has no {} decoder",
                codec.decoder_name().to_string_lossy()
            ),
            Self::OutOfMemory(what) => write!(f, "FFmpeg could not allocate {what}"),
            Self::TooLarge { what, len } => {
                write!(f, "a {len}-byte {what} is too large for FFmpeg")
            }
            Self::InvalidData => f.write_str("FFmpeg rejected the input as invalid data"),
            Self::Ffmpeg {
                operation,
                code,
                reason,
            } => write!(f, "{operation} failed with {code}: {reason}"),
            Self::UnsupportedPixelFormat(name) => write!(
                f,
                "the decoder produced {name} video, but only 8-bit 4:2:0 is supported"
            ),
            Self::UnsupportedSampleFormat(name) => write!(
                f,
                "the decoder produced {name} audio, but only s16, s16p, flt and fltp are supported"
            ),
            Self::MalformedFrame(reason) => write!(f, "FFmpeg returned a frame with {reason}"),
        }
    }
}

impl std::error::Error for DecodeError {}

pub(crate) struct Avcodec {
    functions: Functions,
    soname: &'static str,
    version: LibraryVersion,
    pixel_formats: [(c_int, PixelFormat); 3],
    sample_formats: [(c_int, SampleFormat); 4],
    decoders: Vec<Codec>,
    _avcodec: Library,
    _avutil: Library,
}

impl Avcodec {
    pub(crate) fn soname(&self) -> &'static str {
        self.soname
    }

    pub(crate) fn version(&self) -> LibraryVersion {
        self.version
    }

    fn load(pair: &LibraryPair) -> Result<Option<Self>, BackendUnavailable> {
        let Some(avcodec) = open_library(pair.avcodec) else {
            return Ok(None);
        };
        let Some(avutil) = open_library(pair.avutil) else {
            return Ok(None);
        };

        let avcodec_version: unsafe extern "C" fn() -> c_uint =
            resolve(&avcodec, "avcodec_version")?;
        let avutil_version: unsafe extern "C" fn() -> c_uint = resolve(&avutil, "avutil_version")?;
        let version = LibraryVersion(unsafe { avcodec_version() });
        check_major(pair.avcodec, pair.avcodec_major, version)?;
        check_major(
            pair.avutil,
            pair.avutil_major,
            LibraryVersion(unsafe { avutil_version() }),
        )?;

        let functions = Functions {
            avcodec_find_decoder_by_name: resolve(&avcodec, "avcodec_find_decoder_by_name")?,
            avcodec_alloc_context3: resolve(&avcodec, "avcodec_alloc_context3")?,
            avcodec_free_context: resolve(&avcodec, "avcodec_free_context")?,
            avcodec_parameters_alloc: resolve(&avcodec, "avcodec_parameters_alloc")?,
            avcodec_parameters_free: resolve(&avcodec, "avcodec_parameters_free")?,
            avcodec_parameters_from_context: resolve(&avcodec, "avcodec_parameters_from_context")?,
            avcodec_parameters_to_context: resolve(&avcodec, "avcodec_parameters_to_context")?,
            avcodec_open2: resolve(&avcodec, "avcodec_open2")?,
            avcodec_send_packet: resolve(&avcodec, "avcodec_send_packet")?,
            avcodec_receive_frame: resolve(&avcodec, "avcodec_receive_frame")?,
            avcodec_flush_buffers: resolve(&avcodec, "avcodec_flush_buffers")?,
            av_packet_alloc: resolve(&avcodec, "av_packet_alloc")?,
            av_packet_free: resolve(&avcodec, "av_packet_free")?,
            av_new_packet: resolve(&avcodec, "av_new_packet")?,
            av_packet_unref: resolve(&avcodec, "av_packet_unref")?,
            av_frame_alloc: resolve(&avutil, "av_frame_alloc")?,
            av_frame_free: resolve(&avutil, "av_frame_free")?,
            av_mallocz: resolve(&avutil, "av_mallocz")?,
            av_opt_set_int: resolve(&avutil, "av_opt_set_int")?,
            av_get_pix_fmt: resolve(&avutil, "av_get_pix_fmt")?,
            av_get_pix_fmt_name: resolve(&avutil, "av_get_pix_fmt_name")?,
            av_get_sample_fmt: resolve(&avutil, "av_get_sample_fmt")?,
            av_get_sample_fmt_name: resolve(&avutil, "av_get_sample_fmt_name")?,
            av_strerror: resolve(&avutil, "av_strerror")?,
        };

        let pixel_format = |name: &'static CStr| {
            format_id(name, unsafe { (functions.av_get_pix_fmt)(name.as_ptr()) })
        };
        let pixel_formats = [
            (pixel_format(c"yuv420p")?, PixelFormat::Yuv420p),
            (pixel_format(c"yuvj420p")?, PixelFormat::Yuvj420p),
            (pixel_format(c"nv12")?, PixelFormat::Nv12),
        ];
        let sample_format = |name: &'static CStr| {
            format_id(name, unsafe {
                (functions.av_get_sample_fmt)(name.as_ptr())
            })
        };
        let sample_formats = [
            (sample_format(c"s16")?, SampleFormat::S16),
            (sample_format(c"s16p")?, SampleFormat::S16Planar),
            (sample_format(c"flt")?, SampleFormat::F32),
            (sample_format(c"fltp")?, SampleFormat::F32Planar),
        ];

        let decoders = Codec::ALL
            .into_iter()
            .filter(|codec| {
                let name = codec.decoder_name().as_ptr();
                !unsafe { (functions.avcodec_find_decoder_by_name)(name) }.is_null()
            })
            .collect();

        Ok(Some(Self {
            functions,
            soname: pair.avcodec,
            version,
            pixel_formats,
            sample_formats,
            decoders,
            _avcodec: avcodec,
            _avutil: avutil,
        }))
    }

    fn check(&self, operation: &'static str, code: c_int) -> Result<(), DecodeError> {
        if code >= 0 {
            return Ok(());
        }
        Err(self.failure(operation, code))
    }

    fn failure(&self, operation: &'static str, code: c_int) -> DecodeError {
        let mut text = [0 as c_char; AV_ERROR_MAX_STRING_SIZE];
        unsafe { (self.functions.av_strerror)(code, text.as_mut_ptr(), text.len()) };
        let reason = unsafe { CStr::from_ptr(text.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        DecodeError::Ffmpeg {
            operation,
            code,
            reason,
        }
    }

    fn pixel_format(&self, id: c_int) -> Result<PixelFormat, DecodeError> {
        self.pixel_formats
            .iter()
            .find(|(known, _)| *known == id)
            .map(|(_, format)| *format)
            .ok_or_else(|| {
                let name = unsafe { (self.functions.av_get_pix_fmt_name)(id) };
                DecodeError::UnsupportedPixelFormat(format_name(name, id))
            })
    }

    fn sample_format(&self, id: c_int) -> Result<SampleFormat, DecodeError> {
        self.sample_formats
            .iter()
            .find(|(known, _)| *known == id)
            .map(|(_, format)| *format)
            .ok_or_else(|| {
                let name = unsafe { (self.functions.av_get_sample_fmt_name)(id) };
                DecodeError::UnsupportedSampleFormat(format_name(name, id))
            })
    }
}

fn open_library(soname: &'static str) -> Option<Library> {
    match unsafe { Library::open(Some(soname), RTLD_NOW | RTLD_LOCAL) } {
        Ok(library) => Some(library),
        Err(error) => {
            tracing::debug!(target: "mediandk", soname, %error, "FFmpeg library not loaded");
            None
        }
    }
}

fn resolve<T: Copy>(library: &Library, name: &'static str) -> Result<T, BackendUnavailable> {
    unsafe { library.get::<T>(name.as_bytes()) }
        .map(|symbol| *symbol)
        .map_err(|_| BackendUnavailable::MissingSymbol { name })
}

fn check_major(
    soname: &'static str,
    expected: u32,
    found: LibraryVersion,
) -> Result<(), BackendUnavailable> {
    if found.major() == expected {
        return Ok(());
    }
    Err(BackendUnavailable::VersionMismatch {
        soname,
        expected,
        found,
    })
}

fn format_id(name: &'static CStr, id: c_int) -> Result<c_int, BackendUnavailable> {
    if id < 0 {
        return Err(BackendUnavailable::UnknownFormat { name });
    }
    Ok(id)
}

fn format_name(name: *const c_char, id: c_int) -> String {
    if name.is_null() {
        return format!("format {id}");
    }
    unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .into_owned()
}

pub(crate) struct Backend(Result<Avcodec, BackendUnavailable>);

impl Backend {
    pub(crate) fn load(pairs: &[LibraryPair]) -> Self {
        let mut tried = Vec::with_capacity(pairs.len());
        for pair in pairs {
            match Avcodec::load(pair) {
                Ok(Some(library)) => return Self(Ok(library)),
                Ok(None) => tried.push(pair.avcodec),
                Err(reason) => return Self(Err(reason)),
            }
        }
        Self(Err(BackendUnavailable::NotFound { tried }))
    }

    pub(crate) fn library(&self) -> Result<&Avcodec, &BackendUnavailable> {
        self.0.as_ref()
    }

    pub(crate) fn decoders(&self) -> &[Codec] {
        self.0.as_ref().map_or(&[], |library| &library.decoders)
    }

    fn report(&self) {
        let library = match self.library() {
            Ok(library) => library,
            Err(reason) => {
                tracing::info!(target: "mediandk", "{reason}");
                return;
            }
        };
        tracing::info!(
            target: "mediandk",
            soname = library.soname,
            version = %library.version,
            decoders = ?library.decoders,
            "FFmpeg decoders loaded"
        );
        let missing = missing_codecs_extra(&library.decoders);
        if !missing.is_empty() {
            tracing::info!(target: "mediandk", ?missing, "{CODECS_EXTRA_ADVICE}");
        }
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let library = match self.library() {
            Ok(library) => library,
            Err(reason) => return write!(f, "unavailable: {reason}"),
        };
        write!(f, "{} {} with ", library.soname(), library.version())?;
        if self.decoders().is_empty() {
            return f.write_str("none of Eclipse's decoders");
        }
        let names: Vec<_> = self
            .decoders()
            .iter()
            .map(|codec| codec.decoder_name().to_string_lossy())
            .collect();
        write!(f, "the decoders {}", names.join(", "))
    }
}

pub(crate) fn missing_codecs_extra(decoders: &[Codec]) -> Vec<Codec> {
    Codec::ALL
        .into_iter()
        .filter(|codec| codec.needs_codecs_extra() && !decoders.contains(codec))
        .collect()
}

static BACKEND: OnceLock<Backend> = OnceLock::new();

pub(crate) fn backend() -> &'static Backend {
    BACKEND.get_or_init(|| {
        let backend = Backend::load(&RUNTIME_LIBRARIES);
        backend.report();
        backend
    })
}

pub(crate) fn available_decoders() -> Vec<Codec> {
    if let Some(backend) = BACKEND.get() {
        return backend.decoders().to_vec();
    }
    let probe = Backend::load(&RUNTIME_LIBRARIES);
    probe.report();
    probe.decoders().to_vec()
}

struct FfmpegBox<'lib, T> {
    raw: NonNull<T>,
    free: unsafe extern "C" fn(*mut *mut T),
    library: PhantomData<&'lib Avcodec>,
}

impl<T> FfmpegBox<'_, T> {
    fn new(
        raw: *mut T,
        free: unsafe extern "C" fn(*mut *mut T),
        what: &'static str,
    ) -> Result<Self, DecodeError> {
        let raw = NonNull::new(raw).ok_or(DecodeError::OutOfMemory(what))?;
        Ok(Self {
            raw,
            free,
            library: PhantomData,
        })
    }

    fn as_ptr(&self) -> *mut T {
        self.raw.as_ptr()
    }
}

unsafe impl<T> Send for FfmpegBox<'_, T> {}

impl<T> Drop for FfmpegBox<'_, T> {
    fn drop(&mut self) {
        let mut raw = self.raw.as_ptr();
        unsafe { (self.free)(&mut raw) };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sent {
    Accepted,
    NeedsReceive,
}

pub(crate) enum Received<'lib> {
    Frame(DecodedFrame<'lib>),
    NeedsInput,
    EndOfStream,
}

pub(crate) struct DecoderSession<'lib> {
    library: &'lib Avcodec,
    media: Media,
    context: FfmpegBox<'lib, AvCodecContext>,
    packet: FfmpegBox<'lib, AvPacket>,
    spare_frame: Option<FfmpegBox<'lib, AvFrame>>,
}

impl<'lib> DecoderSession<'lib> {
    pub(crate) fn open(
        library: &'lib Avcodec,
        codec: Codec,
        extradata: &[u8],
    ) -> Result<Self, DecodeError> {
        let functions = &library.functions;
        let decoder =
            unsafe { (functions.avcodec_find_decoder_by_name)(codec.decoder_name().as_ptr()) };
        if decoder.is_null() {
            return Err(DecodeError::DecoderMissing(codec));
        }
        let context = FfmpegBox::new(
            unsafe { (functions.avcodec_alloc_context3)(decoder) },
            functions.avcodec_free_context,
            "a codec context",
        )?;
        let packet = FfmpegBox::new(
            unsafe { (functions.av_packet_alloc)() },
            functions.av_packet_free,
            "a packet",
        )?;
        if !extradata.is_empty() {
            attach_extradata(library, &context, extradata)?;
        }
        let threads = match codec.media() {
            Media::Video => video_decoder_threads(),
            Media::Audio => 1,
        };
        library.check("av_opt_set_int(threads)", unsafe {
            (functions.av_opt_set_int)(context.as_ptr().cast(), c"threads".as_ptr(), threads, 0)
        })?;
        library.check("avcodec_open2", unsafe {
            (functions.avcodec_open2)(context.as_ptr(), decoder, ptr::null_mut())
        })?;
        Ok(Self {
            library,
            media: codec.media(),
            context,
            packet,
            spare_frame: None,
        })
    }

    pub(crate) fn send(
        &mut self,
        data: &[u8],
        pts_us: i64,
        key: bool,
    ) -> Result<Sent, DecodeError> {
        if data.is_empty() {
            return Ok(Sent::Accepted);
        }
        let size = c_int::try_from(data.len()).map_err(|_| DecodeError::TooLarge {
            what: "packet",
            len: data.len(),
        })?;
        let functions = &self.library.functions;
        let packet = self.packet.as_ptr();
        self.library.check("av_new_packet", unsafe {
            (functions.av_new_packet)(packet, size)
        })?;
        let status = unsafe {
            ptr::copy_nonoverlapping(data.as_ptr(), (*packet).data, data.len());
            (*packet).pts = pts_us;
            (*packet).flags = if key { AV_PKT_FLAG_KEY } else { 0 };
            let status = (functions.avcodec_send_packet)(self.context.as_ptr(), packet);
            (functions.av_packet_unref)(packet);
            status
        };
        self.sent(status)
    }

    pub(crate) fn send_end_of_stream(&mut self) -> Result<Sent, DecodeError> {
        let status = unsafe {
            (self.library.functions.avcodec_send_packet)(self.context.as_ptr(), ptr::null())
        };
        self.sent(status)
    }

    fn sent(&self, status: c_int) -> Result<Sent, DecodeError> {
        match status {
            0.. => Ok(Sent::Accepted),
            AVERROR_EAGAIN => Ok(Sent::NeedsReceive),
            AVERROR_INVALIDDATA => Err(DecodeError::InvalidData),
            code => Err(self.library.failure("avcodec_send_packet", code)),
        }
    }

    pub(crate) fn receive(&mut self) -> Result<Received<'lib>, DecodeError> {
        let functions = &self.library.functions;
        let frame = match self.spare_frame.take() {
            Some(frame) => frame,
            None => FfmpegBox::new(
                unsafe { (functions.av_frame_alloc)() },
                functions.av_frame_free,
                "a frame",
            )?,
        };
        let status =
            unsafe { (functions.avcodec_receive_frame)(self.context.as_ptr(), frame.as_ptr()) };
        if status == 0 {
            return DecodedFrame::new(self.library, self.media, frame).map(Received::Frame);
        }
        self.spare_frame = Some(frame);
        match status {
            AVERROR_EAGAIN => Ok(Received::NeedsInput),
            AVERROR_EOF => Ok(Received::EndOfStream),
            AVERROR_INVALIDDATA => Err(DecodeError::InvalidData),
            code => Err(self.library.failure("avcodec_receive_frame", code)),
        }
    }

    pub(crate) fn flush(&mut self) {
        unsafe { (self.library.functions.avcodec_flush_buffers)(self.context.as_ptr()) };
    }
}

fn attach_extradata(
    library: &Avcodec,
    context: &FfmpegBox<'_, AvCodecContext>,
    extradata: &[u8],
) -> Result<(), DecodeError> {
    let functions = &library.functions;
    let size = c_int::try_from(extradata.len()).map_err(|_| DecodeError::TooLarge {
        what: "codec configuration",
        len: extradata.len(),
    })?;
    let parameters = FfmpegBox::new(
        unsafe { (functions.avcodec_parameters_alloc)() },
        functions.avcodec_parameters_free,
        "codec parameters",
    )?;
    library.check("avcodec_parameters_from_context", unsafe {
        (functions.avcodec_parameters_from_context)(parameters.as_ptr(), context.as_ptr())
    })?;
    let copy = unsafe { (functions.av_mallocz)(extradata.len() + AV_INPUT_BUFFER_PADDING_SIZE) }
        .cast::<u8>();
    if copy.is_null() {
        return Err(DecodeError::OutOfMemory("codec configuration"));
    }
    unsafe {
        ptr::copy_nonoverlapping(extradata.as_ptr(), copy, extradata.len());
        (*parameters.as_ptr()).extradata = copy;
        (*parameters.as_ptr()).extradata_size = size;
    }
    library.check("avcodec_parameters_to_context", unsafe {
        (functions.avcodec_parameters_to_context)(context.as_ptr(), parameters.as_ptr())
    })
}

fn video_decoder_threads() -> i64 {
    let available = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
    available.min(MAX_VIDEO_DECODER_THREADS) as i64
}

pub(crate) enum DecodedFrame<'lib> {
    Video(VideoFrame<'lib>),
    Audio(AudioFrame<'lib>),
}

impl<'lib> DecodedFrame<'lib> {
    fn new(
        library: &'lib Avcodec,
        media: Media,
        frame: FfmpegBox<'lib, AvFrame>,
    ) -> Result<Self, DecodeError> {
        match media {
            Media::Video => VideoFrame::new(library, frame).map(DecodedFrame::Video),
            Media::Audio => AudioFrame::new(library, frame).map(DecodedFrame::Audio),
        }
    }

    pub(crate) fn pts_us(&self) -> Option<i64> {
        match self {
            DecodedFrame::Video(video) => video.pts_us(),
            DecodedFrame::Audio(audio) => frame_pts_us(&audio.frame),
        }
    }
}

fn frame_pts_us(frame: &FfmpegBox<'_, AvFrame>) -> Option<i64> {
    let pts = unsafe { (*frame.as_ptr()).pts };
    (pts != AV_NOPTS_VALUE).then_some(pts)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VideoFormat {
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) pixel_format: PixelFormat,
}

impl VideoFormat {
    pub(crate) fn chroma_size(self) -> (usize, usize) {
        (self.width.div_ceil(2), self.height.div_ceil(2))
    }

    pub(crate) fn nv12_len(self) -> usize {
        let Nv12Layout {
            stride,
            slice_height,
        } = Nv12Layout::new(self.width, self.height);
        let (chroma_width, chroma_height) = self.chroma_size();
        stride * slice_height + 2 * chroma_width * chroma_height
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Nv12Layout {
    pub(crate) stride: usize,
    pub(crate) slice_height: usize,
}

impl Nv12Layout {
    pub(crate) fn new(width: usize, height: usize) -> Self {
        Self {
            stride: width.next_multiple_of(2),
            slice_height: height.next_multiple_of(2),
        }
    }
}

pub(crate) struct Plane<'frame> {
    bytes: &'frame [u8],
    stride: usize,
    row_bytes: usize,
}

impl<'frame> Plane<'frame> {
    pub(crate) fn bytes(&self) -> &'frame [u8] {
        self.bytes
    }

    pub(crate) fn stride(&self) -> usize {
        self.stride
    }

    pub(crate) fn rows(&self) -> impl Iterator<Item = &'frame [u8]> + '_ {
        self.bytes
            .chunks(self.stride)
            .map(|row| &row[..self.row_bytes])
    }
}

#[derive(Clone, Copy)]
struct PlaneShape {
    row_bytes: usize,
    rows: usize,
}

pub(crate) struct VideoFrame<'lib> {
    frame: FfmpegBox<'lib, AvFrame>,
    format: VideoFormat,
    strides: [usize; 3],
}

impl<'lib> VideoFrame<'lib> {
    fn new(library: &'lib Avcodec, frame: FfmpegBox<'lib, AvFrame>) -> Result<Self, DecodeError> {
        let raw = frame.as_ptr();
        let (width, height, format) = unsafe { ((*raw).width, (*raw).height, (*raw).format) };
        let pixel_format = library.pixel_format(format)?;
        let (Ok(width @ 1..), Ok(height @ 1..)) = (usize::try_from(width), usize::try_from(height))
        else {
            return Err(DecodeError::MalformedFrame("an empty picture"));
        };
        let format = VideoFormat {
            width,
            height,
            pixel_format,
        };
        let mut strides = [0; 3];
        for (index, shape) in plane_shapes(format).into_iter().enumerate() {
            let Some(shape) = shape else {
                continue;
            };
            let (data, linesize) = unsafe { ((*raw).data[index], (*raw).linesize[index]) };
            if data.is_null() {
                return Err(DecodeError::MalformedFrame("a missing plane"));
            }
            strides[index] = match usize::try_from(linesize) {
                Ok(stride) if stride >= shape.row_bytes => stride,
                Ok(_) => {
                    return Err(DecodeError::MalformedFrame(
                        "a row stride shorter than a row",
                    ))
                }
                Err(_) => return Err(DecodeError::MalformedFrame("a negative row stride")),
            };
        }
        Ok(Self {
            frame,
            format,
            strides,
        })
    }

    fn plane(&self, index: usize) -> Plane<'_> {
        let PlaneShape { row_bytes, rows } = plane_shapes(self.format)[index]
            .unwrap_or_else(|| panic!("{:?} has no plane {index}", self.format.pixel_format));
        let stride = self.strides[index];
        let data = unsafe { (*self.frame.as_ptr()).data[index] };
        Plane {
            bytes: unsafe { std::slice::from_raw_parts(data, stride * (rows - 1) + row_bytes) },
            stride,
            row_bytes,
        }
    }

    pub(crate) fn format(&self) -> VideoFormat {
        self.format
    }

    pub(crate) fn pts_us(&self) -> Option<i64> {
        frame_pts_us(&self.frame)
    }

    pub(crate) fn luma(&self) -> Plane<'_> {
        self.plane(0)
    }

    pub(crate) fn write_nv12(&self, out: &mut Vec<u8>) {
        let VideoFormat { width, height, .. } = self.format;
        let Nv12Layout {
            stride,
            slice_height,
        } = Nv12Layout::new(width, height);
        out.clear();
        out.reserve(self.format.nv12_len());
        for row in self.luma().rows() {
            out.extend_from_slice(row);
            out.resize(out.len() + stride - width, 0);
        }
        out.resize(stride * slice_height, 0);
        self.extend_with_interleaved_chroma(out);
    }

    pub(crate) fn extend_with_interleaved_chroma(&self, out: &mut Vec<u8>) {
        match self.format.pixel_format {
            PixelFormat::Nv12 => {
                for row in self.plane(1).rows() {
                    out.extend_from_slice(row);
                }
            }
            PixelFormat::Yuv420p | PixelFormat::Yuvj420p => {
                let (chroma_width, chroma_height) = self.format.chroma_size();
                let start = out.len();
                out.resize(start + 2 * chroma_width * chroma_height, 0);
                let (u, v) = (self.plane(1), self.plane(2));
                let rows = out[start..].chunks_exact_mut(2 * chroma_width);
                for ((row, u_row), v_row) in rows.zip(u.rows()).zip(v.rows()) {
                    let pairs = row.as_chunks_mut::<2>().0.iter_mut();
                    for ((pair, &u), &v) in pairs.zip(u_row).zip(v_row) {
                        *pair = [u, v];
                    }
                }
            }
        }
    }
}

fn plane_shapes(format: VideoFormat) -> [Option<PlaneShape>; 3] {
    let (chroma_width, chroma_height) = format.chroma_size();
    let luma = PlaneShape {
        row_bytes: format.width,
        rows: format.height,
    };
    let chroma = PlaneShape {
        row_bytes: chroma_width,
        rows: chroma_height,
    };
    match format.pixel_format {
        PixelFormat::Nv12 => [
            Some(luma),
            Some(PlaneShape {
                row_bytes: 2 * chroma_width,
                ..chroma
            }),
            None,
        ],
        PixelFormat::Yuv420p | PixelFormat::Yuvj420p => [Some(luma), Some(chroma), Some(chroma)],
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AudioFormat {
    pub(crate) sample_rate: u32,
    pub(crate) channels: usize,
}

pub(crate) struct AudioFrame<'lib> {
    frame: FfmpegBox<'lib, AvFrame>,
    format: AudioFormat,
    sample_format: SampleFormat,
    samples: usize,
}

impl<'lib> AudioFrame<'lib> {
    fn new(library: &'lib Avcodec, frame: FfmpegBox<'lib, AvFrame>) -> Result<Self, DecodeError> {
        let raw = frame.as_ptr();
        let (format, sample_rate, channels, samples, planes) = unsafe {
            (
                (*raw).format,
                (*raw).sample_rate,
                (*raw).ch_layout_nb_channels,
                (*raw).nb_samples,
                (*raw).extended_data,
            )
        };
        let sample_format = library.sample_format(format)?;
        let (Ok(sample_rate @ 1..), Ok(channels @ 1..), Ok(samples)) = (
            u32::try_from(sample_rate),
            usize::try_from(channels),
            usize::try_from(samples),
        ) else {
            return Err(DecodeError::MalformedFrame("no sample rate or channels"));
        };
        let plane_count = if sample_format.planar() { channels } else { 1 };
        let alignment = match sample_format {
            SampleFormat::S16 | SampleFormat::S16Planar => align_of::<i16>(),
            SampleFormat::F32 | SampleFormat::F32Planar => align_of::<f32>(),
        };
        if samples > 0 {
            if planes.is_null() {
                return Err(DecodeError::MalformedFrame("no sample planes"));
            }
            for index in 0..plane_count {
                let plane = unsafe { *planes.add(index) };
                if plane.is_null() || plane.align_offset(alignment) != 0 {
                    return Err(DecodeError::MalformedFrame(
                        "a missing or misaligned sample plane",
                    ));
                }
            }
        }
        Ok(Self {
            frame,
            format: AudioFormat {
                sample_rate,
                channels,
            },
            sample_format,
            samples,
        })
    }

    pub(crate) fn format(&self) -> AudioFormat {
        self.format
    }

    fn interleave<T: Copy>(&self, mut sample: impl FnMut(T)) {
        if self.samples == 0 {
            return;
        }
        let channels = self.format.channels;
        let planes = unsafe { (*self.frame.as_ptr()).extended_data };
        if !self.sample_format.planar() {
            let first = unsafe { *planes }.cast::<T>();
            let all = unsafe { std::slice::from_raw_parts(first, self.samples * channels) };
            all.iter().copied().for_each(sample);
            return;
        }
        let planes: Vec<&[T]> = (0..channels)
            .map(|channel| unsafe {
                std::slice::from_raw_parts((*planes.add(channel)).cast::<T>(), self.samples)
            })
            .collect();
        for index in 0..self.samples {
            for plane in &planes {
                sample(plane[index]);
            }
        }
    }

    pub(crate) fn write_pcm(&self, encoding: PcmEncoding, out: &mut Vec<u8>) {
        out.clear();
        let width = match encoding {
            PcmEncoding::I16 => size_of::<i16>(),
            PcmEncoding::F32 => size_of::<f32>(),
        };
        out.reserve(self.samples * self.format.channels * width);
        match (self.sample_format, encoding) {
            (SampleFormat::S16 | SampleFormat::S16Planar, PcmEncoding::I16) => {
                self.interleave(|sample: i16| out.extend_from_slice(&sample.to_ne_bytes()));
            }
            (SampleFormat::S16 | SampleFormat::S16Planar, PcmEncoding::F32) => {
                self.interleave(|sample: i16| {
                    out.extend_from_slice(&(f32::from(sample) / 32768.0).to_ne_bytes());
                });
            }
            (SampleFormat::F32 | SampleFormat::F32Planar, PcmEncoding::F32) => {
                self.interleave(|sample: f32| out.extend_from_slice(&sample.to_ne_bytes()));
            }
            (SampleFormat::F32 | SampleFormat::F32Planar, PcmEncoding::I16) => {
                self.interleave(|sample: f32| {
                    out.extend_from_slice(&float_to_i16(sample).to_ne_bytes());
                });
            }
        }
    }
}

fn float_to_i16(sample: f32) -> i16 {
    (sample * 32768.0)
        .round()
        .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}

#[cfg(test)]
mod tests;
