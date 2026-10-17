use std::collections::VecDeque;
use std::ffi::{c_char, c_void, CStr, CString};
use std::fmt;
use std::mem::offset_of;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tracing::field::DisplayValue;

use super::avcodec::{
    self, AudioFormat, Avcodec, Backend, BackendUnavailable, Codec, DecodeError, DecodedFrame,
    DecoderSession, Media, Nv12Layout, PcmEncoding, Received, Sent, VideoFormat, VideoFrame,
};
use super::format::{self, FormatValue, MediaFormat};
use super::image_reader::{self, ReaderError};
use super::{
    c_text, MediaStatus, AMEDIA_ERROR_INVALID_OBJECT, AMEDIA_ERROR_INVALID_OPERATION,
    AMEDIA_ERROR_INVALID_PARAMETER, AMEDIA_ERROR_UNKNOWN, AMEDIA_ERROR_UNSUPPORTED, AMEDIA_OK,
    KEY_CHANNEL_COUNT, KEY_COLOR_FORMAT, KEY_HEIGHT, KEY_MIME, KEY_SAMPLE_RATE, KEY_STRIDE,
    KEY_WIDTH,
};
use crate::loader::ndk_registry::{
    self, handle_to_ptr, ptr_to_handle, NdkHandle, NdkRegistryError, Slab,
};

pub(super) const AMEDIACODEC_INFO_TRY_AGAIN_LATER: isize = -1;
pub(super) const AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED: isize = -2;

const AMEDIACODEC_CONFIGURE_FLAG_ENCODE: u32 = 1;

const AMEDIACODEC_BUFFER_FLAG_KEY_FRAME: u32 = 1;
const AMEDIACODEC_BUFFER_FLAG_CODEC_CONFIG: u32 = 2;
pub(super) const AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM: u32 = 4;

pub(super) const COLOR_FORMAT_YUV420_SEMI_PLANAR: i32 = 21;
pub(super) const COLOR_FORMAT_SURFACE: i32 = 0x7F00_0789;
pub(super) const PCM_ENCODING_16BIT: i32 = 2;
const PCM_ENCODING_FLOAT: i32 = 4;

const KEY_CROP_BOTTOM: &CStr = c"crop-bottom";
const KEY_CROP_LEFT: &CStr = c"crop-left";
const KEY_CROP_RIGHT: &CStr = c"crop-right";
const KEY_CROP_TOP: &CStr = c"crop-top";
pub(super) const KEY_CSD_0: &CStr = c"csd-0";
pub(super) const KEY_CSD_1: &CStr = c"csd-1";
const KEY_MAX_INPUT_SIZE: &CStr = c"max-input-size";
pub(super) const KEY_PCM_ENCODING: &CStr = c"pcm-encoding";
pub(super) const KEY_SLICE_HEIGHT: &CStr = c"slice-height";

const MIME_AUDIO_RAW: &CStr = c"audio/raw";
const MIME_VIDEO_RAW: &CStr = c"video/raw";

const MAX_LIVE_CODECS: usize = 16;
const MAX_DIMENSION: i32 = 4096;
const INPUT_SLOTS: usize = 4;
const OUTPUT_SLOTS: usize = 4;
const MIN_VIDEO_INPUT_CAPACITY: usize = 2 << 20;
const AUDIO_INPUT_CAPACITY: usize = 64 << 10;
const MAX_INPUT_CAPACITY: usize = MAX_DIMENSION as usize * MAX_DIMENSION as usize * 3 / 2;
const MAX_REPORTED_MIMES: usize = 32;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct BufferInfo {
    pub(super) offset: i32,
    pub(super) size: i32,
    pub(super) presentation_time_us: i64,
    pub(super) flags: u32,
}

const _: () = {
    assert!(offset_of!(BufferInfo, offset) == 0);
    assert!(offset_of!(BufferInfo, size) == 4);
    assert!(offset_of!(BufferInfo, presentation_time_us) == 8);
    assert!(offset_of!(BufferInfo, flags) == 16);
    assert!(size_of::<BufferInfo>() == 24);
};

#[derive(Debug)]
enum CodecError {
    NotACodec(NdkRegistryError),
    Deleted,
    Failed,
    Poisoned,
    WrongState {
        call: &'static str,
        state: &'static str,
    },
    Encoder,
    Crypto,
    Surface(*mut c_void),
    Format(NdkRegistryError),
    BadSize {
        width: Option<i32>,
        height: Option<i32>,
    },
    TooLarge {
        width: i32,
        height: i32,
    },
    PcmEncoding(i32),
    InputCapacity(i32),
    NotOwned(usize),
    OutOfRange {
        offset: libc::off_t,
        size: usize,
        capacity: usize,
    },
    AfterEndOfStream,
    NullInfo,
    ReaderOutput,
    Render(ReaderError),
    Registry(NdkRegistryError),
}

impl CodecError {
    fn status(&self) -> MediaStatus {
        match self {
            Self::NotACodec(_) | Self::Deleted => AMEDIA_ERROR_INVALID_OBJECT,
            Self::Failed | Self::Poisoned | Self::Render(_) | Self::Registry(_) => {
                AMEDIA_ERROR_UNKNOWN
            }
            Self::Encoder
            | Self::Crypto
            | Self::Surface(_)
            | Self::TooLarge { .. }
            | Self::PcmEncoding(_) => AMEDIA_ERROR_UNSUPPORTED,
            Self::Format(_)
            | Self::BadSize { .. }
            | Self::InputCapacity(_)
            | Self::NotOwned(_)
            | Self::OutOfRange { .. }
            | Self::NullInfo => AMEDIA_ERROR_INVALID_PARAMETER,
            Self::WrongState { .. } | Self::AfterEndOfStream | Self::ReaderOutput => {
                AMEDIA_ERROR_INVALID_OPERATION
            }
        }
    }
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotACodec(error) => write!(f, "not a live AMediaCodec: {error}"),
            Self::Deleted => f.write_str("the codec was deleted"),
            Self::Failed => f.write_str("the codec failed earlier"),
            Self::Poisoned => f.write_str("the codec lock was poisoned"),
            Self::WrongState { call, state } => write!(f, "{call} is invalid while {state}"),
            Self::Encoder => f.write_str("Eclipse has no encoders"),
            Self::Crypto => f.write_str("encrypted media is unsupported"),
            Self::Surface(surface) => write!(f, "{surface:?} is not a surface Eclipse can fill"),
            Self::Format(error) => write!(f, "not a live AMediaFormat: {error}"),
            Self::BadSize { width, height } => {
                write!(f, "video needs a positive size, not {width:?}x{height:?}")
            }
            Self::TooLarge { width, height } => write!(
                f,
                "{width}x{height} exceeds {MAX_DIMENSION}x{MAX_DIMENSION}"
            ),
            Self::PcmEncoding(encoding) => write!(f, "PCM encoding {encoding} is unsupported"),
            Self::InputCapacity(size) => write!(
                f,
                "max-input-size {size} is outside 0 to {MAX_INPUT_CAPACITY}"
            ),
            Self::NotOwned(index) => write!(f, "buffer {index} is not held by the caller"),
            Self::OutOfRange {
                offset,
                size,
                capacity,
            } => write!(
                f,
                "{size} bytes at offset {offset} overrun a {capacity}-byte buffer"
            ),
            Self::AfterEndOfStream => f.write_str("the input already ended"),
            Self::NullInfo => f.write_str("the buffer info pointer is NULL"),
            Self::ReaderOutput => f.write_str("outputs go to an AImageReader and have no buffer"),
            Self::Render(error) => write!(f, "the frame could not be rendered: {error}"),
            Self::Registry(error) => error.fmt(f),
        }
    }
}

fn status_of<T>(result: &Result<T, CodecError>) -> MediaStatus {
    result
        .as_ref()
        .map_or_else(CodecError::status, |_| AMEDIA_OK)
}

fn failure<T>(result: &Result<T, CodecError>) -> Option<DisplayValue<&CodecError>> {
    result.as_ref().err().map(tracing::field::display)
}

#[derive(Debug)]
pub(super) enum Refusal {
    NoMime,
    UnknownMime,
    Unavailable(&'static BackendUnavailable),
    NoDecoder { codec: Codec, soname: &'static str },
    TooManyCodecs,
    Registry(NdkRegistryError),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoMime => f.write_str("no MIME type was given"),
            Self::UnknownMime => f.write_str("Eclipse has no decoder for this MIME type"),
            Self::Unavailable(reason) => reason.fmt(f),
            Self::NoDecoder { codec, soname } => {
                write!(
                    f,
                    "{soname} has no {} decoder",
                    codec.decoder_name().to_string_lossy()
                )?;
                if codec.needs_codecs_extra() {
                    write!(f, "; {}", avcodec::CODECS_EXTRA_ADVICE)?;
                }
                Ok(())
            }
            Self::TooManyCodecs => write!(f, "{MAX_LIVE_CODECS} codecs are already live"),
            Self::Registry(error) => error.fmt(f),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputMode {
    Buffers,
    ImageReader(NdkHandle),
}

impl OutputMode {
    fn color_format(self) -> i32 {
        match self {
            Self::Buffers => COLOR_FORMAT_YUV420_SEMI_PLANAR,
            Self::ImageReader(_) => COLOR_FORMAT_SURFACE,
        }
    }
}

#[derive(Debug)]
enum Track {
    Video {
        width: usize,
        height: usize,
        output: OutputMode,
    },
    Audio {
        pcm: PcmEncoding,
        sample_rate: Option<i32>,
        channels: Option<i32>,
    },
}

#[derive(Debug)]
struct Config {
    track: Track,
    csd: Vec<u8>,
    input_capacity: usize,
}

impl Config {
    fn parse(codec: Codec, format: &MediaFormat, output: OutputMode) -> Result<Self, CodecError> {
        let (track, default_capacity) = match codec.media() {
            Media::Video => {
                let (width, height) = video_size(format)?;
                let capacity = (width * height * 3 / 2).max(MIN_VIDEO_INPUT_CAPACITY);
                let track = Track::Video {
                    width,
                    height,
                    output,
                };
                (track, capacity)
            }
            Media::Audio => {
                let pcm = match format.int32(KEY_PCM_ENCODING) {
                    None | Some(PCM_ENCODING_16BIT) => PcmEncoding::I16,
                    Some(PCM_ENCODING_FLOAT) => PcmEncoding::F32,
                    Some(other) => return Err(CodecError::PcmEncoding(other)),
                };
                let track = Track::Audio {
                    pcm,
                    sample_rate: format.int32(KEY_SAMPLE_RATE),
                    channels: format.int32(KEY_CHANNEL_COUNT),
                };
                (track, AUDIO_INPUT_CAPACITY)
            }
        };
        let input_capacity = match format.int32(KEY_MAX_INPUT_SIZE) {
            None | Some(0) => default_capacity,
            Some(size) => match usize::try_from(size) {
                Ok(size) if size <= MAX_INPUT_CAPACITY => size.max(default_capacity),
                Ok(_) | Err(_) => return Err(CodecError::InputCapacity(size)),
            },
        };
        let csd = [KEY_CSD_0, KEY_CSD_1]
            .into_iter()
            .filter_map(|key| format.buffer(key))
            .flatten()
            .copied()
            .collect();
        Ok(Self {
            track,
            csd,
            input_capacity,
        })
    }
}

impl Track {
    fn output_entries(&self, reported: Option<OutputFormat>) -> Vec<(&'static CStr, FormatValue)> {
        match *self {
            Track::Video {
                width,
                height,
                output,
            } => match reported {
                Some(OutputFormat::Video(video)) => {
                    video_entries(video.width, video.height, output)
                }
                Some(OutputFormat::Audio(_)) | None => video_entries(width, height, output),
            },
            Track::Audio {
                pcm,
                sample_rate,
                channels,
            } => match reported {
                Some(OutputFormat::Audio(audio)) => audio_entries(
                    pcm,
                    Some(format_int(audio.sample_rate)),
                    Some(format_int(audio.channels)),
                ),
                Some(OutputFormat::Video(_)) | None => audio_entries(pcm, sample_rate, channels),
            },
        }
    }
}

fn video_size(format: &MediaFormat) -> Result<(usize, usize), CodecError> {
    let (width, height) = (format.int32(KEY_WIDTH), format.int32(KEY_HEIGHT));
    let (Some(width @ 1..), Some(height @ 1..)) = (width, height) else {
        return Err(CodecError::BadSize { width, height });
    };
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(CodecError::TooLarge { width, height });
    }
    Ok((
        width.unsigned_abs() as usize,
        height.unsigned_abs() as usize,
    ))
}

fn format_int(value: impl TryInto<i32>) -> i32 {
    value
        .try_into()
        .unwrap_or_else(|_| panic!("decoded sizes and rates come from C ints"))
}

fn video_entries(
    width: usize,
    height: usize,
    output: OutputMode,
) -> Vec<(&'static CStr, FormatValue)> {
    let layout = Nv12Layout::new(width, height);
    vec![
        (KEY_MIME, FormatValue::String(MIME_VIDEO_RAW.to_owned())),
        (KEY_WIDTH, FormatValue::Int32(format_int(width))),
        (KEY_HEIGHT, FormatValue::Int32(format_int(height))),
        (KEY_STRIDE, FormatValue::Int32(format_int(layout.stride))),
        (
            KEY_SLICE_HEIGHT,
            FormatValue::Int32(format_int(layout.slice_height)),
        ),
        (KEY_COLOR_FORMAT, FormatValue::Int32(output.color_format())),
        (KEY_CROP_LEFT, FormatValue::Int32(0)),
        (KEY_CROP_TOP, FormatValue::Int32(0)),
        (KEY_CROP_RIGHT, FormatValue::Int32(format_int(width - 1))),
        (KEY_CROP_BOTTOM, FormatValue::Int32(format_int(height - 1))),
    ]
}

fn audio_entries(
    pcm: PcmEncoding,
    sample_rate: Option<i32>,
    channels: Option<i32>,
) -> Vec<(&'static CStr, FormatValue)> {
    let encoding = match pcm {
        PcmEncoding::I16 => PCM_ENCODING_16BIT,
        PcmEncoding::F32 => PCM_ENCODING_FLOAT,
    };
    let mut entries = vec![
        (KEY_MIME, FormatValue::String(MIME_AUDIO_RAW.to_owned())),
        (KEY_PCM_ENCODING, FormatValue::Int32(encoding)),
    ];
    entries.extend(sample_rate.map(|rate| (KEY_SAMPLE_RATE, FormatValue::Int32(rate))));
    entries.extend(channels.map(|count| (KEY_CHANNEL_COUNT, FormatValue::Int32(count))));
    entries
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputFormat {
    Video(VideoFormat),
    Audio(AudioFormat),
}

impl OutputFormat {
    fn of(frame: &DecodedFrame<'_>) -> Self {
        match frame {
            DecodedFrame::Video(video) => Self::Video(video.format()),
            DecodedFrame::Audio(audio) => Self::Audio(audio.format()),
        }
    }
}

#[derive(Clone, Copy)]
struct QueuedInput {
    offset: usize,
    size: usize,
    pts_us: i64,
    flags: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InputState {
    Free,
    Dequeued,
    Queued,
}

struct InputSlot {
    state: InputState,
    buffer: Vec<u8>,
}

enum Output {
    Frame(DecodedFrame<'static>),
    EndOfStream { pts_us: i64 },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputState {
    Free,
    Ready,
    Held,
}

struct OutputSlot {
    state: OutputState,
    bytes: Vec<u8>,
    frame: Option<VideoFrame<'static>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stream {
    Open,
    Closing { pts_us: i64 },
    Draining { pts_us: i64 },
    Ended,
}

enum Dequeued {
    FormatChanged,
    Buffer { index: usize, info: BufferInfo },
}

struct Running {
    track: Track,
    input_capacity: usize,
    extradata: Vec<u8>,
    session: Option<DecoderSession<'static>>,
    inputs: [InputSlot; INPUT_SLOTS],
    queued: VecDeque<(usize, QueuedInput)>,
    outputs: [OutputSlot; OUTPUT_SLOTS],
    ready: VecDeque<(usize, Output)>,
    stream: Stream,
    reported: Option<OutputFormat>,
}

impl Running {
    fn new(config: Config) -> Self {
        let Config {
            track,
            csd,
            input_capacity,
        } = config;
        Self {
            track,
            input_capacity,
            extradata: csd,
            session: None,
            inputs: std::array::from_fn(|_| InputSlot {
                state: InputState::Free,
                buffer: Vec::new(),
            }),
            queued: VecDeque::with_capacity(INPUT_SLOTS),
            outputs: std::array::from_fn(|_| OutputSlot {
                state: OutputState::Free,
                bytes: Vec::new(),
                frame: None,
            }),
            ready: VecDeque::with_capacity(OUTPUT_SLOTS),
            stream: Stream::Open,
            reported: None,
        }
    }

    fn pump(&mut self, library: &'static Avcodec, codec: Codec) -> Result<bool, DecodeError> {
        let mut progressed = false;
        loop {
            let received = self.receive()?;
            let sent = self.send(library, codec)?;
            if !received && !sent {
                return Ok(progressed);
            }
            progressed = true;
        }
    }

    fn receive(&mut self) -> Result<bool, DecodeError> {
        let Some(session) = &mut self.session else {
            return Ok(false);
        };
        let mut received = false;
        while self.stream != Stream::Ended {
            let Some(index) = self
                .outputs
                .iter()
                .position(|slot| slot.state == OutputState::Free)
            else {
                break;
            };
            let output = match session.receive() {
                Ok(Received::Frame(frame)) => Output::Frame(frame),
                Ok(Received::NeedsInput) => break,
                Ok(Received::EndOfStream) => {
                    let pts_us = match self.stream {
                        Stream::Closing { pts_us } | Stream::Draining { pts_us } => pts_us,
                        Stream::Open | Stream::Ended => 0,
                    };
                    self.stream = Stream::Ended;
                    Output::EndOfStream { pts_us }
                }
                Err(DecodeError::InvalidData) => {
                    tracing::debug!(target: "mediandk", "the decoder dropped a corrupt frame");
                    continue;
                }
                Err(error) => return Err(error),
            };
            self.outputs[index].state = OutputState::Ready;
            self.ready.push_back((index, output));
            received = true;
        }
        Ok(received)
    }

    fn send(&mut self, library: &'static Avcodec, codec: Codec) -> Result<bool, DecodeError> {
        let mut sent = false;
        while let Some(&(index, input)) = self.queued.front() {
            let bytes = &self.inputs[index].buffer[input.offset..input.offset + input.size];
            let config = input.flags & AMEDIACODEC_BUFFER_FLAG_CODEC_CONFIG != 0;
            match (config, self.session.is_some(), codec.media()) {
                (true, false, Media::Video) => self.extradata.extend_from_slice(bytes),
                (true, false, Media::Audio) => {
                    self.extradata.clear();
                    self.extradata.extend_from_slice(bytes);
                }
                (true, true, Media::Audio) => tracing::debug!(
                    target: "mediandk",
                    size = input.size,
                    "audio codec configuration after the first frame is ignored"
                ),
                (false, _, _) | (true, true, Media::Video) => {
                    let session = open_session(&mut self.session, library, codec, &self.extradata)?;
                    let key = input.flags & AMEDIACODEC_BUFFER_FLAG_KEY_FRAME != 0;
                    match session.send(bytes, input.pts_us, key) {
                        Ok(Sent::Accepted) => {}
                        Ok(Sent::NeedsReceive) => return Ok(sent),
                        Err(DecodeError::InvalidData) => tracing::debug!(
                            target: "mediandk",
                            index,
                            size = input.size,
                            pts_us = input.pts_us,
                            "the decoder dropped a corrupt input"
                        ),
                        Err(error) => return Err(error),
                    }
                }
            }
            self.queued.pop_front();
            self.inputs[index].state = InputState::Free;
            sent = true;
        }
        if let Stream::Closing { pts_us } = self.stream {
            let session = open_session(&mut self.session, library, codec, &self.extradata)?;
            if session.send_end_of_stream()? == Sent::Accepted {
                self.stream = Stream::Draining { pts_us };
                sent = true;
            }
        }
        Ok(sent)
    }

    fn take_input(&mut self) -> Option<usize> {
        let index = self
            .inputs
            .iter()
            .position(|slot| slot.state == InputState::Free)?;
        let slot = &mut self.inputs[index];
        if slot.buffer.is_empty() {
            slot.buffer = vec![0; self.input_capacity];
        }
        slot.state = InputState::Dequeued;
        Some(index)
    }

    fn queue_input(
        &mut self,
        index: usize,
        offset: libc::off_t,
        size: usize,
        pts_us: i64,
        flags: u32,
    ) -> Result<(), CodecError> {
        if self.stream != Stream::Open {
            return Err(CodecError::AfterEndOfStream);
        }
        let slot = self
            .inputs
            .get_mut(index)
            .filter(|slot| slot.state == InputState::Dequeued)
            .ok_or(CodecError::NotOwned(index))?;
        let capacity = slot.buffer.len();
        let start = usize::try_from(offset)
            .ok()
            .filter(|start| start.checked_add(size).is_some_and(|end| end <= capacity))
            .ok_or(CodecError::OutOfRange {
                offset,
                size,
                capacity,
            })?;
        slot.state = InputState::Queued;
        self.queued.push_back((
            index,
            QueuedInput {
                offset: start,
                size,
                pts_us,
                flags,
            },
        ));
        if flags & AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM != 0 {
            self.stream = Stream::Closing { pts_us };
        }
        Ok(())
    }

    fn take_output(&mut self) -> Result<Option<Dequeued>, DecodeError> {
        let Some((_, output)) = self.ready.front() else {
            return Ok(None);
        };
        if let Output::Frame(frame) = output {
            let format = OutputFormat::of(frame);
            if self.reported != Some(format) {
                self.reported = Some(format);
                return Ok(Some(Dequeued::FormatChanged));
            }
        }
        let Some((index, output)) = self.ready.pop_front() else {
            return Ok(None);
        };
        let slot = &mut self.outputs[index];
        let (len, presentation_time_us, flags) = match output {
            Output::Frame(frame) => {
                let pts_us = frame.pts_us().unwrap_or(0);
                let len = match (frame, &self.track) {
                    (
                        DecodedFrame::Video(video),
                        Track::Video {
                            output: OutputMode::Buffers,
                            ..
                        },
                    ) => {
                        video.write_nv12(&mut slot.bytes);
                        slot.bytes.len()
                    }
                    (
                        DecodedFrame::Video(video),
                        Track::Video {
                            output: OutputMode::ImageReader(_),
                            ..
                        },
                    ) => {
                        let len = video.format().nv12_len();
                        slot.frame = Some(video);
                        len
                    }
                    (DecodedFrame::Audio(audio), Track::Audio { pcm, .. }) => {
                        audio.write_pcm(*pcm, &mut slot.bytes);
                        slot.bytes.len()
                    }
                    (DecodedFrame::Audio(_), Track::Video { .. }) => {
                        return Err(DecodeError::MalformedFrame("audio from a video codec"));
                    }
                    (DecodedFrame::Video(_), Track::Audio { .. }) => {
                        return Err(DecodeError::MalformedFrame("video from an audio codec"));
                    }
                };
                (len, pts_us, 0)
            }
            Output::EndOfStream { pts_us } => {
                slot.bytes.clear();
                (0, pts_us, AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM)
            }
        };
        let size = i32::try_from(len).map_err(|_| DecodeError::TooLarge {
            what: "decoded output",
            len,
        })?;
        slot.state = OutputState::Held;
        Ok(Some(Dequeued::Buffer {
            index,
            info: BufferInfo {
                offset: 0,
                size,
                presentation_time_us,
                flags,
            },
        }))
    }

    fn release_output(
        &mut self,
        index: usize,
        render: bool,
    ) -> Result<Option<(NdkHandle, VideoFrame<'static>)>, CodecError> {
        let slot = self
            .outputs
            .get_mut(index)
            .filter(|slot| slot.state == OutputState::Held)
            .ok_or(CodecError::NotOwned(index))?;
        slot.state = OutputState::Free;
        let frame = slot.frame.take().filter(|_| render);
        let Track::Video {
            output: OutputMode::ImageReader(reader),
            ..
        } = self.track
        else {
            return Ok(None);
        };
        Ok(frame.map(|frame| (reader, frame)))
    }

    fn flush(&mut self) {
        if let Some(session) = &mut self.session {
            session.flush();
        }
        for slot in &mut self.inputs {
            slot.state = InputState::Free;
        }
        self.queued.clear();
        for slot in &mut self.outputs {
            slot.state = OutputState::Free;
            slot.frame = None;
        }
        self.ready.clear();
        self.stream = Stream::Open;
    }
}

fn open_session<'a>(
    session: &'a mut Option<DecoderSession<'static>>,
    library: &'static Avcodec,
    codec: Codec,
    extradata: &[u8],
) -> Result<&'a mut DecoderSession<'static>, DecodeError> {
    match session {
        Some(session) => Ok(session),
        None => Ok(session.insert(DecoderSession::open(library, codec, extradata)?)),
    }
}

enum Phase {
    Unconfigured,
    Configured(Config),
    Started(Box<Running>),
    Failed,
    Deleted,
}

impl Phase {
    fn refuse(&self, call: &'static str) -> CodecError {
        let state = match self {
            Self::Failed => return CodecError::Failed,
            Self::Deleted => return CodecError::Deleted,
            Self::Unconfigured => "unconfigured",
            Self::Configured(_) => "configured",
            Self::Started(_) => "started",
        };
        CodecError::WrongState { call, state }
    }

    fn running(&mut self, call: &'static str) -> Result<&mut Running, CodecError> {
        match self {
            Self::Started(running) => Ok(running),
            other => Err(other.refuse(call)),
        }
    }

    fn fail(&mut self, error: DecodeError) -> CodecError {
        tracing::warn!(target: "mediandk", %error, "media codec failed");
        *self = Self::Failed;
        CodecError::Failed
    }

    fn pump(
        &mut self,
        library: &'static Avcodec,
        codec: Codec,
        call: &'static str,
    ) -> Result<bool, CodecError> {
        let pumped = self.running(call)?.pump(library, codec);
        pumped.map_err(|error| self.fail(error))
    }
}

enum Wait {
    Forever,
    Until(Instant),
}

impl Wait {
    fn from_timeout(timeout_us: i64) -> Self {
        let Ok(timeout_us) = u64::try_from(timeout_us) else {
            return Self::Forever;
        };
        Instant::now()
            .checked_add(Duration::from_micros(timeout_us))
            .map_or(Self::Forever, Self::Until)
    }
}

struct CodecShared {
    codec: Codec,
    library: &'static Avcodec,
    phase: Mutex<Phase>,
    changed: Condvar,
}

impl CodecShared {
    fn lock(&self) -> Result<MutexGuard<'_, Phase>, CodecError> {
        self.phase.lock().map_err(|_| CodecError::Poisoned)
    }

    fn configure(
        &self,
        format: *const c_void,
        surface: *mut c_void,
        crypto: *mut c_void,
        flags: u32,
    ) -> Result<(), CodecError> {
        if flags & AMEDIACODEC_CONFIGURE_FLAG_ENCODE != 0 {
            return Err(CodecError::Encoder);
        }
        if !crypto.is_null() {
            return Err(CodecError::Crypto);
        }
        let output = self.output_mode(surface)?;
        let config = format::read(format, |media_format| {
            Config::parse(self.codec, media_format, output)
        })
        .map_err(CodecError::Format)??;
        let mut phase = self.lock()?;
        match &*phase {
            Phase::Unconfigured => {
                *phase = Phase::Configured(config);
                Ok(())
            }
            other => Err(other.refuse("AMediaCodec_configure")),
        }
    }

    fn output_mode(&self, surface: *mut c_void) -> Result<OutputMode, CodecError> {
        if surface.is_null() {
            return Ok(OutputMode::Buffers);
        }
        match (
            self.codec.media(),
            ndk_registry::reader_window(surface as usize),
        ) {
            (Media::Video, Some(window)) => Ok(OutputMode::ImageReader(window.reader)),
            (Media::Video | Media::Audio, _) => Err(CodecError::Surface(surface)),
        }
    }

    fn start(&self) -> Result<(), CodecError> {
        let mut phase = self.lock()?;
        match std::mem::replace(&mut *phase, Phase::Unconfigured) {
            Phase::Configured(config) => {
                *phase = Phase::Started(Box::new(Running::new(config)));
                Ok(())
            }
            other => {
                let error = other.refuse("AMediaCodec_start");
                *phase = other;
                Err(error)
            }
        }
    }

    fn stop(&self) -> Result<(), CodecError> {
        let mut phase = self.lock()?;
        match &*phase {
            Phase::Configured(_) | Phase::Started(_) => *phase = Phase::Unconfigured,
            other => return Err(other.refuse("AMediaCodec_stop")),
        }
        drop(phase);
        self.changed.notify_all();
        Ok(())
    }

    fn flush(&self) -> Result<(), CodecError> {
        self.lock()?.running("AMediaCodec_flush")?.flush();
        self.changed.notify_all();
        Ok(())
    }

    fn wait_for<T>(
        &self,
        call: &'static str,
        timeout_us: i64,
        mut take: impl FnMut(&mut Running) -> Result<Option<T>, DecodeError>,
    ) -> Result<Option<T>, CodecError> {
        let wait = Wait::from_timeout(timeout_us);
        let mut phase = self.lock()?;
        loop {
            let attempt = phase
                .pump(self.library, self.codec, call)
                .and_then(|progressed| {
                    if progressed {
                        self.changed.notify_all();
                    }
                    let taken = take(phase.running(call)?);
                    taken.map_err(|error| phase.fail(error))
                });
            match attempt {
                Ok(Some(found)) => return Ok(Some(found)),
                Ok(None) => {}
                Err(error) => {
                    self.changed.notify_all();
                    return Err(error);
                }
            }
            phase = match wait {
                Wait::Forever => self.changed.wait(phase).map_err(|_| CodecError::Poisoned)?,
                Wait::Until(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Ok(None);
                    }
                    self.changed
                        .wait_timeout(phase, left)
                        .map_err(|_| CodecError::Poisoned)?
                        .0
                }
            };
        }
    }

    fn dequeue_input(&self, timeout_us: i64) -> Result<Option<usize>, CodecError> {
        self.wait_for("AMediaCodec_dequeueInputBuffer", timeout_us, |running| {
            Ok(running.take_input())
        })
    }

    fn input_buffer(&self, index: usize) -> Result<(*mut u8, usize), CodecError> {
        let mut phase = self.lock()?;
        let slot = phase
            .running("AMediaCodec_getInputBuffer")?
            .inputs
            .get_mut(index)
            .filter(|slot| slot.state == InputState::Dequeued)
            .ok_or(CodecError::NotOwned(index))?;
        Ok((slot.buffer.as_mut_ptr(), slot.buffer.len()))
    }

    fn queue_input(
        &self,
        index: usize,
        offset: libc::off_t,
        size: usize,
        pts_us: i64,
        flags: u32,
    ) -> Result<(), CodecError> {
        self.lock()?
            .running("AMediaCodec_queueInputBuffer")?
            .queue_input(index, offset, size, pts_us, flags)?;
        self.changed.notify_all();
        Ok(())
    }

    fn dequeue_output(&self, timeout_us: i64) -> Result<Option<Dequeued>, CodecError> {
        self.wait_for(
            "AMediaCodec_dequeueOutputBuffer",
            timeout_us,
            Running::take_output,
        )
    }

    fn output_buffer(&self, index: usize) -> Result<(*mut u8, usize), CodecError> {
        let mut phase = self.lock()?;
        let running = phase.running("AMediaCodec_getOutputBuffer")?;
        if let Track::Video {
            output: OutputMode::ImageReader(_),
            ..
        } = running.track
        {
            return Err(CodecError::ReaderOutput);
        }
        let slot = running
            .outputs
            .get_mut(index)
            .filter(|slot| slot.state == OutputState::Held)
            .ok_or(CodecError::NotOwned(index))?;
        Ok((slot.bytes.as_mut_ptr(), slot.bytes.len()))
    }

    fn release_output(&self, index: usize, render: bool) -> Result<(), CodecError> {
        let rendered = self
            .lock()?
            .running("AMediaCodec_releaseOutputBuffer")?
            .release_output(index, render)?;
        self.changed.notify_all();
        let Some((reader, frame)) = rendered else {
            return Ok(());
        };
        image_reader::render(reader, frame).map_err(CodecError::Render)
    }

    fn output_format(&self) -> Result<Vec<(&'static CStr, FormatValue)>, CodecError> {
        match &*self.lock()? {
            Phase::Configured(config) => Ok(config.track.output_entries(None)),
            Phase::Started(running) => Ok(running.track.output_entries(running.reported)),
            other => Err(other.refuse("AMediaCodec_getOutputFormat")),
        }
    }
}

struct Codecs {
    slab: Slab<Arc<CodecShared>>,
    live: AtomicUsize,
}

impl Codecs {
    const fn new() -> Self {
        Self {
            slab: Slab::new(),
            live: AtomicUsize::new(0),
        }
    }

    fn create_decoder(
        &self,
        backend: impl FnOnce() -> &'static Backend,
        mime: &CStr,
    ) -> Result<NdkHandle, Refusal> {
        let (codec, library) = decoder_for(backend, mime)?;
        self.live
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |live| {
                (live < MAX_LIVE_CODECS).then_some(live + 1)
            })
            .map_err(|_| Refusal::TooManyCodecs)?;
        let shared = Arc::new(CodecShared {
            codec,
            library,
            phase: Mutex::new(Phase::Unconfigured),
            changed: Condvar::new(),
        });
        self.slab.insert(shared).map_err(|error| {
            self.live.fetch_sub(1, Ordering::SeqCst);
            Refusal::Registry(error)
        })
    }

    fn get(&self, codec: *mut c_void) -> Result<Arc<CodecShared>, CodecError> {
        self.slab
            .with(ptr_to_handle(codec), |shared| Arc::clone(shared))
            .map_err(CodecError::NotACodec)
    }

    fn delete(&self, codec: *mut c_void) -> Result<(), CodecError> {
        let shared = self.get(codec)?;
        self.slab
            .remove(ptr_to_handle(codec))
            .map_err(CodecError::NotACodec)?;
        self.live.fetch_sub(1, Ordering::SeqCst);
        let mut phase = shared.phase.lock().unwrap_or_else(PoisonError::into_inner);
        *phase = Phase::Deleted;
        drop(phase);
        shared.changed.notify_all();
        Ok(())
    }
}

pub(super) fn decoder_for(
    backend: impl FnOnce() -> &'static Backend,
    mime: &CStr,
) -> Result<(Codec, &'static Avcodec), Refusal> {
    let codec = Codec::for_mime(mime.to_bytes()).ok_or(Refusal::UnknownMime)?;
    let backend = backend();
    let library = backend.library().map_err(Refusal::Unavailable)?;
    if !backend.decoders().contains(&codec) {
        return Err(Refusal::NoDecoder {
            codec,
            soname: library.soname(),
        });
    }
    Ok((codec, library))
}

fn codecs() -> &'static Codecs {
    static CODECS: Codecs = Codecs::new();
    &CODECS
}

fn report_refusal(mime: Option<&CStr>, refusal: &Refusal) {
    static REPORTED: Mutex<Vec<CString>> = Mutex::new(Vec::new());
    let key = mime.unwrap_or_default();
    let mut reported = REPORTED.lock().unwrap_or_else(PoisonError::into_inner);
    if reported.len() >= MAX_REPORTED_MIMES || reported.iter().any(|seen| seen.as_c_str() == key) {
        return;
    }
    reported.push(key.to_owned());
    drop(reported);
    tracing::info!(
        target: "mediandk",
        mime = %key.to_string_lossy(),
        "AMediaCodec_createDecoderByType returns NULL: {refusal}"
    );
}

pub(super) unsafe extern "C" fn eclipse_amediacodec_createdecoderbytype(
    mime_type: *const c_char,
) -> *mut c_void {
    let mime = unsafe { c_text(mime_type) };
    let created = match mime {
        Some(mime) => codecs().create_decoder(avcodec::backend, mime),
        None => Err(Refusal::NoMime),
    };
    let codec = match &created {
        Ok(handle) => handle_to_ptr(*handle),
        Err(refusal) => {
            report_refusal(mime, refusal);
            std::ptr::null_mut()
        }
    };
    tracing::debug!(
        target: "mediandk",
        ?mime,
        ?codec,
        refusal = ?created.err(),
        "AMediaCodec_createDecoderByType"
    );
    codec
}

pub(super) unsafe extern "C" fn eclipse_amediacodec_createencoderbytype(
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

pub(super) unsafe extern "C" fn eclipse_amediacodec_configure(
    codec: *mut c_void,
    format: *const c_void,
    surface: *mut c_void,
    crypto: *mut c_void,
    flags: u32,
) -> MediaStatus {
    let shared = codecs().get(codec);
    let mime = shared.as_ref().ok().map(|shared| shared.codec.mime());
    let description = format::read(format, MediaFormat::summary).ok();
    let result = shared.and_then(|shared| shared.configure(format, surface, crypto, flags));
    let status = status_of(&result);
    tracing::debug!(
        target: "mediandk",
        ?codec,
        ?mime,
        ?format,
        ?description,
        ?surface,
        ?crypto,
        flags,
        status,
        error = failure(&result),
        "AMediaCodec_configure"
    );
    status
}

fn lifecycle_call(
    call: &'static str,
    codec: *mut c_void,
    action: impl FnOnce(&CodecShared) -> Result<(), CodecError>,
) -> MediaStatus {
    let result = codecs().get(codec).and_then(|shared| action(&shared));
    let status = status_of(&result);
    tracing::debug!(
        target: "mediandk",
        ?codec,
        status,
        error = failure(&result),
        "{call}"
    );
    status
}

pub(super) unsafe extern "C" fn eclipse_amediacodec_start(codec: *mut c_void) -> MediaStatus {
    lifecycle_call("AMediaCodec_start", codec, CodecShared::start)
}

pub(super) unsafe extern "C" fn eclipse_amediacodec_stop(codec: *mut c_void) -> MediaStatus {
    lifecycle_call("AMediaCodec_stop", codec, CodecShared::stop)
}

pub(super) unsafe extern "C" fn eclipse_amediacodec_flush(codec: *mut c_void) -> MediaStatus {
    lifecycle_call("AMediaCodec_flush", codec, CodecShared::flush)
}

pub(super) unsafe extern "C" fn eclipse_amediacodec_delete(codec: *mut c_void) -> MediaStatus {
    let result = codecs().delete(codec);
    let status = status_of(&result);
    tracing::debug!(
        target: "mediandk",
        ?codec,
        status,
        error = failure(&result),
        "AMediaCodec_delete"
    );
    status
}

pub(super) unsafe extern "C" fn eclipse_amediacodec_dequeueinputbuffer(
    codec: *mut c_void,
    timeout_us: i64,
) -> isize {
    let result = codecs()
        .get(codec)
        .and_then(|shared| shared.dequeue_input(timeout_us));
    let index = match &result {
        Ok(Some(index)) => *index as isize,
        Ok(None) => AMEDIACODEC_INFO_TRY_AGAIN_LATER,
        Err(error) => error.status() as isize,
    };
    tracing::trace!(
        target: "mediandk",
        ?codec,
        timeout_us,
        index,
        error = failure(&result),
        "AMediaCodec_dequeueInputBuffer"
    );
    index
}

pub(super) unsafe extern "C" fn eclipse_amediacodec_getinputbuffer(
    codec: *mut c_void,
    index: usize,
    out_size: *mut usize,
) -> *mut u8 {
    let result = codecs()
        .get(codec)
        .and_then(|shared| shared.input_buffer(index));
    let (buffer, size) = match &result {
        Ok((buffer, size)) => (*buffer, Some(*size)),
        Err(_) => (std::ptr::null_mut(), None),
    };
    if let (Some(size), false) = (size, out_size.is_null()) {
        unsafe { out_size.write(size) };
    }
    tracing::trace!(
        target: "mediandk",
        ?codec,
        index,
        ?buffer,
        ?size,
        error = failure(&result),
        "AMediaCodec_getInputBuffer"
    );
    buffer
}

pub(super) unsafe extern "C" fn eclipse_amediacodec_queueinputbuffer(
    codec: *mut c_void,
    index: usize,
    offset: libc::off_t,
    size: usize,
    pts_us: u64,
    flags: u32,
) -> MediaStatus {
    let result = codecs()
        .get(codec)
        .and_then(|shared| shared.queue_input(index, offset, size, pts_us as i64, flags));
    let status = status_of(&result);
    tracing::trace!(
        target: "mediandk",
        ?codec,
        index,
        offset,
        size,
        pts_us,
        flags,
        status,
        error = failure(&result),
        "AMediaCodec_queueInputBuffer"
    );
    status
}

pub(super) unsafe extern "C" fn eclipse_amediacodec_dequeueoutputbuffer(
    codec: *mut c_void,
    info: *mut c_void,
    timeout_us: i64,
) -> isize {
    let info = info.cast::<BufferInfo>();
    let result = codecs().get(codec).and_then(|shared| {
        if info.is_null() {
            return Err(CodecError::NullInfo);
        }
        shared.dequeue_output(timeout_us)
    });
    let (index, written) = match &result {
        Ok(Some(Dequeued::Buffer {
            index,
            info: filled,
        })) => {
            unsafe { info.write(*filled) };
            (*index as isize, Some(*filled))
        }
        Ok(Some(Dequeued::FormatChanged)) => (AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED, None),
        Ok(None) => (AMEDIACODEC_INFO_TRY_AGAIN_LATER, None),
        Err(error) => (error.status() as isize, None),
    };
    tracing::trace!(
        target: "mediandk",
        ?codec,
        timeout_us,
        index,
        size = written.map(|info| info.size),
        pts_us = written.map(|info| info.presentation_time_us),
        flags = written.map(|info| info.flags),
        error = failure(&result),
        "AMediaCodec_dequeueOutputBuffer"
    );
    index
}

pub(super) unsafe extern "C" fn eclipse_amediacodec_getoutputbuffer(
    codec: *mut c_void,
    index: usize,
    out_size: *mut usize,
) -> *mut u8 {
    let result = codecs()
        .get(codec)
        .and_then(|shared| shared.output_buffer(index));
    let (buffer, size) = match &result {
        Ok((buffer, size)) => (*buffer, Some(*size)),
        Err(_) => (std::ptr::null_mut(), None),
    };
    if let (Some(size), false) = (size, out_size.is_null()) {
        unsafe { out_size.write(size) };
    }
    tracing::trace!(
        target: "mediandk",
        ?codec,
        index,
        ?buffer,
        ?size,
        error = failure(&result),
        "AMediaCodec_getOutputBuffer"
    );
    buffer
}

pub(super) unsafe extern "C" fn eclipse_amediacodec_releaseoutputbuffer(
    codec: *mut c_void,
    index: usize,
    render: bool,
) -> MediaStatus {
    let result = codecs()
        .get(codec)
        .and_then(|shared| shared.release_output(index, render));
    let status = status_of(&result);
    tracing::trace!(
        target: "mediandk",
        ?codec,
        index,
        render,
        status,
        error = failure(&result),
        "AMediaCodec_releaseOutputBuffer"
    );
    status
}

pub(super) unsafe extern "C" fn eclipse_amediacodec_getoutputformat(
    codec: *mut c_void,
) -> *mut c_void {
    let result = codecs()
        .get(codec)
        .and_then(|shared| shared.output_format())
        .and_then(|entries| format::create(entries).map_err(CodecError::Registry));
    let format = result
        .as_ref()
        .map_or(std::ptr::null_mut(), |format| *format);
    tracing::debug!(
        target: "mediandk",
        ?codec,
        ?format,
        error = failure(&result),
        "AMediaCodec_getOutputFormat"
    );
    format
}

fn encoder_only_call(call: &'static str, codec: *mut c_void) -> MediaStatus {
    let status = match codecs().get(codec) {
        Ok(_) => AMEDIA_ERROR_INVALID_OPERATION,
        Err(error) => error.status(),
    };
    tracing::debug!(target: "mediandk", ?codec, status, "{call}");
    status
}

pub(super) unsafe extern "C" fn eclipse_amediacodec_createinputsurface(
    codec: *mut c_void,
    _surface: *mut *mut c_void,
) -> MediaStatus {
    encoder_only_call("AMediaCodec_createInputSurface", codec)
}

pub(super) unsafe extern "C" fn eclipse_amediacodec_signalendofinputstream(
    codec: *mut c_void,
) -> MediaStatus {
    encoder_only_call("AMediaCodec_signalEndOfInputStream", codec)
}

#[cfg(test)]
pub(super) mod tests;
