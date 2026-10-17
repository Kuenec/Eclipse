use std::collections::BTreeSet;
use std::ffi::{c_int, c_void, CStr};
use std::fmt;
use std::path::Path;
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use ring::digest::{Context, SHA256};

use super::avcodec::{self, Backend};
use super::codec::{
    self, BufferInfo, AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM,
    AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED, AMEDIACODEC_INFO_TRY_AGAIN_LATER, COLOR_FORMAT_SURFACE,
    COLOR_FORMAT_YUV420_SEMI_PLANAR, KEY_CSD_0, KEY_CSD_1, KEY_PCM_ENCODING, KEY_SLICE_HEIGHT,
    PCM_ENCODING_16BIT,
};
use super::fixture::{self, AudioTrack, Fixture, Track, VideoCheck, VideoTrack};
use super::format;
use super::image_reader::{self, ImageListenerDesc, AIMAGE_FORMAT_YUV_420_888};
use super::{
    MediaStatus, AMEDIA_OK, KEY_CHANNEL_COUNT, KEY_COLOR_FORMAT, KEY_HEIGHT, KEY_SAMPLE_RATE,
    KEY_STRIDE, KEY_WIDTH,
};
use crate::loader::native_provider::EclipseNativeProvider;
use crate::loader::resolve::SymbolProvider;

const STALL_LIMIT: Duration = Duration::from_secs(10);
const IDLE_WAIT_US: i64 = 1_000;
const LISTENER_WAIT: Duration = Duration::from_secs(5);
const READER_MAX_IMAGES: i32 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Buffers,
    ImageReader,
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Buffers => "buffers",
            Self::ImageReader => "an image reader",
        })
    }
}

struct FrameTimes {
    total: Duration,
    per_frame: Vec<Duration>,
}

impl fmt::Display for FrameTimes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut sorted = self.per_frame.clone();
        sorted.sort_unstable();
        let Some(&max) = sorted.last() else {
            return f.write_str("no frames timed");
        };
        let percentile = |share: f64| {
            let rank = ((sorted.len() - 1) as f64 * share).round() as usize;
            sorted[rank].as_secs_f64() * 1000.0
        };
        let seconds = self.total.as_secs_f64();
        write!(
            f,
            "{} frames in {seconds:.2} s, {:.1} a second; per frame median {:.2} ms, \
             p90 {:.2} ms, p99 {:.2} ms, max {:.2} ms",
            sorted.len(),
            sorted.len() as f64 / seconds,
            percentile(0.5),
            percentile(0.9),
            percentile(0.99),
            max.as_secs_f64() * 1000.0
        )
    }
}

struct FrameTimer {
    started: Instant,
    last: Instant,
    per_frame: Vec<Duration>,
}

impl FrameTimer {
    fn start() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            last: now,
            per_frame: Vec::new(),
        }
    }

    fn frame(&mut self) {
        let now = Instant::now();
        self.per_frame.push(now - self.last);
        self.last = now;
    }

    fn finish(self) -> FrameTimes {
        FrameTimes {
            total: self.last - self.started,
            per_frame: self.per_frame,
        }
    }
}

enum Outcome {
    Passed {
        modes: &'static [Mode],
        timing: Option<FrameTimes>,
    },
    NoDecoder(String),
    Wrong {
        mode: Mode,
        problem: String,
    },
}

struct FixtureReport {
    mime: String,
    fixture: String,
    outcome: Outcome,
}

impl fmt::Display for FixtureReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            mime,
            fixture,
            outcome,
        } = self;
        match outcome {
            Outcome::Passed { modes, timing } => {
                let modes: Vec<String> = modes.iter().map(Mode::to_string).collect();
                write!(f, "{mime}: ok ({fixture} through {})", modes.join(" and "))?;
                match timing {
                    Some(timing) => write!(f, "; {timing}"),
                    None => Ok(()),
                }
            }
            Outcome::NoDecoder(reason) => write!(f, "{mime}: no decoder: {reason}"),
            Outcome::Wrong { mode, problem } => {
                write!(f, "{mime}: wrong ({fixture} through {mode}): {problem}")
            }
        }
    }
}

pub struct MediaTestReport {
    backend: String,
    results: Vec<FixtureReport>,
}

impl MediaTestReport {
    pub fn passed(&self) -> bool {
        !self
            .results
            .iter()
            .any(|result| matches!(result.outcome, Outcome::Wrong { .. }))
    }
}

impl fmt::Display for MediaTestReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "backend: {}", self.backend)?;
        for result in &self.results {
            writeln!(f, "{result}")?;
        }
        Ok(())
    }
}

pub fn run(dir: &Path) -> Result<MediaTestReport, String> {
    run_with(dir, avcodec::backend)
}

fn run_with(dir: &Path, backend: fn() -> &'static Backend) -> Result<MediaTestReport, String> {
    let fixtures = Fixture::read_all(dir).map_err(|error| error.to_string())?;
    check_registration()?;
    let results = fixtures
        .iter()
        .map(|fixture| FixtureReport {
            mime: fixture.mime.to_string_lossy().into_owned(),
            fixture: fixture.name().to_owned(),
            outcome: test_fixture(fixture, backend),
        })
        .collect();
    Ok(MediaTestReport {
        backend: backend().to_string(),
        results,
    })
}

fn check_registration() -> Result<(), String> {
    let used: [(&str, *const ()); 28] = [
        (
            "AMediaCodec_createDecoderByType",
            codec::eclipse_amediacodec_createdecoderbytype as *const (),
        ),
        (
            "AMediaCodec_configure",
            codec::eclipse_amediacodec_configure as *const (),
        ),
        (
            "AMediaCodec_start",
            codec::eclipse_amediacodec_start as *const (),
        ),
        (
            "AMediaCodec_delete",
            codec::eclipse_amediacodec_delete as *const (),
        ),
        (
            "AMediaCodec_dequeueInputBuffer",
            codec::eclipse_amediacodec_dequeueinputbuffer as *const (),
        ),
        (
            "AMediaCodec_getInputBuffer",
            codec::eclipse_amediacodec_getinputbuffer as *const (),
        ),
        (
            "AMediaCodec_queueInputBuffer",
            codec::eclipse_amediacodec_queueinputbuffer as *const (),
        ),
        (
            "AMediaCodec_dequeueOutputBuffer",
            codec::eclipse_amediacodec_dequeueoutputbuffer as *const (),
        ),
        (
            "AMediaCodec_getOutputBuffer",
            codec::eclipse_amediacodec_getoutputbuffer as *const (),
        ),
        (
            "AMediaCodec_releaseOutputBuffer",
            codec::eclipse_amediacodec_releaseoutputbuffer as *const (),
        ),
        (
            "AMediaCodec_getOutputFormat",
            codec::eclipse_amediacodec_getoutputformat as *const (),
        ),
        (
            "AMediaFormat_new",
            format::eclipse_amediaformat_new as *const (),
        ),
        (
            "AMediaFormat_delete",
            format::eclipse_amediaformat_delete as *const (),
        ),
        (
            "AMediaFormat_setInt32",
            format::eclipse_amediaformat_setint32 as *const (),
        ),
        (
            "AMediaFormat_setBuffer",
            format::eclipse_amediaformat_setbuffer as *const (),
        ),
        (
            "AMediaFormat_getInt32",
            format::eclipse_amediaformat_getint32 as *const (),
        ),
        (
            "AImageReader_new",
            image_reader::eclipse_aimagereader_new as *const (),
        ),
        (
            "AImageReader_getWindow",
            image_reader::eclipse_aimagereader_getwindow as *const (),
        ),
        (
            "AImageReader_setImageListener",
            image_reader::eclipse_aimagereader_setimagelistener as *const (),
        ),
        (
            "AImageReader_acquireLatestImage",
            image_reader::eclipse_aimagereader_acquirelatestimage as *const (),
        ),
        (
            "AImageReader_delete",
            image_reader::eclipse_aimagereader_delete as *const (),
        ),
        (
            "AImage_getFormat",
            image_reader::eclipse_aimage_getformat as *const (),
        ),
        (
            "AImage_getCropRect",
            image_reader::eclipse_aimage_getcroprect as *const (),
        ),
        (
            "AImage_getTimestamp",
            image_reader::eclipse_aimage_gettimestamp as *const (),
        ),
        (
            "AImage_getPlaneData",
            image_reader::eclipse_aimage_getplanedata as *const (),
        ),
        (
            "AImage_getPlaneRowStride",
            image_reader::eclipse_aimage_getplanerowstride as *const (),
        ),
        (
            "AImage_getPlanePixelStride",
            image_reader::eclipse_aimage_getplanepixelstride as *const (),
        ),
        (
            "AImage_delete",
            image_reader::eclipse_aimage_delete as *const (),
        ),
    ];
    let provider = EclipseNativeProvider::with_bionic_natives();
    for (name, function) in used {
        match provider.resolve(name) {
            Some(symbol) if symbol.addr == function as u64 => {}
            Some(symbol) => {
                return Err(format!(
                    "{name} resolves to {:#x}, not to Eclipse's media native",
                    symbol.addr
                ))
            }
            None => return Err(format!("{name} is not an Eclipse native")),
        }
    }
    Ok(())
}

fn test_fixture(fixture: &Fixture, backend: fn() -> &'static Backend) -> Outcome {
    if let Err(refusal) = codec::decoder_for(backend, &fixture.mime) {
        return Outcome::NoDecoder(refusal.to_string());
    }
    match &fixture.track {
        Track::Audio(audio) => match decode_audio(fixture, audio) {
            Ok(()) => Outcome::Passed {
                modes: &[Mode::Buffers],
                timing: None,
            },
            Err(problem) => Outcome::Wrong {
                mode: Mode::Buffers,
                problem,
            },
        },
        Track::Video(video) => test_video(fixture, video),
    }
}

fn test_video(fixture: &Fixture, video: &VideoTrack) -> Outcome {
    let timing = match decode_video_buffers(fixture, video) {
        Ok(timing) => timing,
        Err(problem) => {
            return Outcome::Wrong {
                mode: Mode::Buffers,
                problem,
            }
        }
    };
    if let VideoCheck::Timed = video.check {
        return Outcome::Passed {
            modes: &[Mode::Buffers],
            timing: Some(timing),
        };
    }
    match decode_video_reader(fixture, video) {
        Ok(()) => Outcome::Passed {
            modes: &[Mode::Buffers, Mode::ImageReader],
            timing: None,
        },
        Err(problem) => Outcome::Wrong {
            mode: Mode::ImageReader,
            problem,
        },
    }
}

fn status(call: &str, status: MediaStatus) -> Result<(), String> {
    if status == AMEDIA_OK {
        return Ok(());
    }
    Err(format!("{call} returned {status}"))
}

fn c_int_of(what: &str, value: usize) -> Result<i32, String> {
    i32::try_from(value).map_err(|_| format!("{what} {value} does not fit an int"))
}

struct Format(*mut c_void);

impl Format {
    fn new() -> Result<Self, String> {
        let format = format::eclipse_amediaformat_new();
        if format.is_null() {
            return Err("AMediaFormat_new returned NULL".to_owned());
        }
        Ok(Self(format))
    }

    fn set_int32(&self, key: &CStr, value: usize) -> Result<(), String> {
        let value = c_int_of(&key.to_string_lossy(), value)?;
        unsafe { format::eclipse_amediaformat_setint32(self.0, key.as_ptr(), value) };
        Ok(())
    }

    fn set_buffer(&self, key: &CStr, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        unsafe {
            format::eclipse_amediaformat_setbuffer(
                self.0,
                key.as_ptr(),
                bytes.as_ptr().cast(),
                bytes.len(),
            );
        }
    }

    fn int32(&self, key: &CStr) -> Option<i32> {
        let mut value = 0;
        unsafe { format::eclipse_amediaformat_getint32(self.0, key.as_ptr(), &mut value) }
            .then_some(value)
    }

    fn expect(&self, key: &CStr, expected: i32) -> Result<(), String> {
        match self.int32(key) {
            Some(found) if found == expected => Ok(()),
            found => Err(format!(
                "the output format's {} is {found:?}, not {expected}",
                key.to_string_lossy()
            )),
        }
    }

    fn at_least(&self, key: &CStr, minimum: usize) -> Result<usize, String> {
        self.int32(key)
            .and_then(|found| usize::try_from(found).ok())
            .filter(|&found| found >= minimum)
            .ok_or_else(|| {
                format!(
                    "the output format's {} is {:?}, below {minimum}",
                    key.to_string_lossy(),
                    self.int32(key)
                )
            })
    }
}

impl Drop for Format {
    fn drop(&mut self) {
        unsafe { format::eclipse_amediaformat_delete(self.0) };
    }
}

enum Dequeued {
    TryAgain,
    FormatChanged,
    Buffer { index: usize, info: BufferInfo },
}

struct Decoder(*mut c_void);

impl Decoder {
    fn configured(fixture: &Fixture, surface: *mut c_void) -> Result<Self, String> {
        let codec =
            unsafe { codec::eclipse_amediacodec_createdecoderbytype(fixture.mime.as_ptr()) };
        if codec.is_null() {
            return Err("AMediaCodec_createDecoderByType returned NULL".to_owned());
        }
        let decoder = Self(codec);
        let format = Format::new()?;
        match &fixture.track {
            Track::Video(video) => {
                format.set_int32(KEY_WIDTH, video.width)?;
                format.set_int32(KEY_HEIGHT, video.height)?;
            }
            Track::Audio(audio) => {
                format.set_int32(KEY_SAMPLE_RATE, audio.sample_rate as usize)?;
                format.set_int32(KEY_CHANNEL_COUNT, audio.channels)?;
            }
        }
        format.set_buffer(KEY_CSD_0, fixture.csd_0());
        format.set_buffer(KEY_CSD_1, fixture.csd_1());
        status("AMediaCodec_configure", unsafe {
            codec::eclipse_amediacodec_configure(
                decoder.0,
                format.0,
                surface,
                std::ptr::null_mut(),
                0,
            )
        })?;
        status("AMediaCodec_start", unsafe {
            codec::eclipse_amediacodec_start(decoder.0)
        })?;
        Ok(decoder)
    }

    fn queue(&self, bytes: &[u8], pts_us: i64, flags: u32) -> Result<bool, String> {
        let index = unsafe { codec::eclipse_amediacodec_dequeueinputbuffer(self.0, 0) };
        if index == AMEDIACODEC_INFO_TRY_AGAIN_LATER {
            return Ok(false);
        }
        let index = usize::try_from(index)
            .map_err(|_| format!("AMediaCodec_dequeueInputBuffer returned {index}"))?;
        let mut capacity = 0;
        let buffer =
            unsafe { codec::eclipse_amediacodec_getinputbuffer(self.0, index, &mut capacity) };
        if buffer.is_null() || capacity < bytes.len() {
            return Err(format!(
                "input buffer {index} holds {capacity} bytes, less than a {}-byte packet",
                bytes.len()
            ));
        }
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, bytes.len()) };
        status("AMediaCodec_queueInputBuffer", unsafe {
            codec::eclipse_amediacodec_queueinputbuffer(
                self.0,
                index,
                0,
                bytes.len(),
                pts_us as u64,
                flags,
            )
        })?;
        Ok(true)
    }

    fn dequeue_output(&self, timeout_us: i64) -> Result<Dequeued, String> {
        let mut info = BufferInfo::default();
        let index = unsafe {
            codec::eclipse_amediacodec_dequeueoutputbuffer(
                self.0,
                (&raw mut info).cast(),
                timeout_us,
            )
        };
        match index {
            AMEDIACODEC_INFO_TRY_AGAIN_LATER => Ok(Dequeued::TryAgain),
            AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED => Ok(Dequeued::FormatChanged),
            index => usize::try_from(index)
                .map(|index| Dequeued::Buffer { index, info })
                .map_err(|_| format!("AMediaCodec_dequeueOutputBuffer returned {index}")),
        }
    }

    fn take_output<R>(
        &self,
        index: usize,
        info: &BufferInfo,
        read: impl FnOnce(&[u8]) -> Result<R, String>,
    ) -> Result<R, String> {
        let mut capacity = 0;
        let buffer =
            unsafe { codec::eclipse_amediacodec_getoutputbuffer(self.0, index, &mut capacity) };
        let range = usize::try_from(info.offset)
            .ok()
            .zip(usize::try_from(info.size).ok())
            .filter(|&(offset, size)| offset.checked_add(size).is_some_and(|end| end <= capacity));
        let (Some((offset, size)), false) = (range, buffer.is_null()) else {
            return Err(format!(
                "output buffer {index} ({capacity} bytes) does not hold {} bytes at {}",
                info.size, info.offset
            ));
        };
        let result = read(unsafe { std::slice::from_raw_parts(buffer.add(offset), size) });
        self.release(index, false)?;
        result
    }

    fn release(&self, index: usize, render: bool) -> Result<(), String> {
        status("AMediaCodec_releaseOutputBuffer", unsafe {
            codec::eclipse_amediacodec_releaseoutputbuffer(self.0, index, render)
        })
    }

    fn output_format(&self) -> Result<Format, String> {
        let format = unsafe { codec::eclipse_amediacodec_getoutputformat(self.0) };
        if format.is_null() {
            return Err("AMediaCodec_getOutputFormat returned NULL".to_owned());
        }
        Ok(Format(format))
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { codec::eclipse_amediacodec_delete(self.0) };
    }
}

enum Event {
    FormatChanged,
    Frame { index: usize, info: BufferInfo },
}

fn drive(
    fixture: &Fixture,
    decoder: &Decoder,
    mut handle: impl FnMut(Event) -> Result<(), String>,
) -> Result<(), String> {
    let packets = fixture.packets.len();
    let mut next = 0;
    let mut input_ended = false;
    let mut progress = Instant::now();
    loop {
        let mut fed = false;
        while !input_ended {
            let queued = match fixture.packets.get(next) {
                Some(packet) => decoder.queue(fixture.packet(next), packet.pts_us, packet.flags)?,
                None => decoder.queue(&[], 0, AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM)?,
            };
            if !queued {
                break;
            }
            fed = true;
            input_ended = next == packets;
            next += 1;
        }
        match decoder.dequeue_output(if fed { 0 } else { IDLE_WAIT_US })? {
            Dequeued::TryAgain if fed => {}
            Dequeued::TryAgain => {
                if progress.elapsed() > STALL_LIMIT {
                    return Err(format!(
                        "the decoder stalled after {} of {packets} packets",
                        next.min(packets)
                    ));
                }
                continue;
            }
            Dequeued::FormatChanged => handle(Event::FormatChanged)?,
            Dequeued::Buffer { index, info }
                if info.flags & AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM != 0 =>
            {
                return decoder.release(index, false);
            }
            Dequeued::Buffer { index, info } => handle(Event::Frame { index, info })?,
        }
        progress = Instant::now();
    }
}

#[derive(Clone, Copy)]
struct BufferLayout {
    stride: usize,
    slice_height: usize,
}

fn video_output(
    format: &Format,
    video: &VideoTrack,
    color_format: i32,
) -> Result<BufferLayout, String> {
    format.expect(KEY_WIDTH, c_int_of("width", video.width)?)?;
    format.expect(KEY_HEIGHT, c_int_of("height", video.height)?)?;
    format.expect(KEY_COLOR_FORMAT, color_format)?;
    Ok(BufferLayout {
        stride: format.at_least(KEY_STRIDE, video.width)?,
        slice_height: format.at_least(KEY_SLICE_HEIGHT, video.height)?,
    })
}

fn chroma_size(video: &VideoTrack) -> (usize, usize) {
    (video.width.div_ceil(2), video.height.div_ceil(2))
}

fn buffer_nv12(bytes: &[u8], video: &VideoTrack, layout: BufferLayout) -> Result<Vec<u8>, String> {
    let (chroma_width, chroma_height) = chroma_size(video);
    let BufferLayout {
        stride,
        slice_height,
    } = layout;
    let chroma_start = stride * slice_height;
    let mut packed =
        Vec::with_capacity(video.width * video.height + 2 * chroma_width * chroma_height);
    let rows = (0..video.height)
        .map(|row| (row * stride, video.width))
        .chain((0..chroma_height).map(|row| (chroma_start + row * stride, 2 * chroma_width)));
    for (start, len) in rows {
        let row = bytes.get(start..start + len).ok_or_else(|| {
            format!(
                "a {}-byte NV12 buffer ends before byte {}",
                bytes.len(),
                start + len
            )
        })?;
        packed.extend_from_slice(row);
    }
    Ok(packed)
}

fn check_video_frames(
    fixture: &Fixture,
    video: &VideoTrack,
    frames: &[(i64, String)],
    stream: Context,
) -> Result<(), String> {
    if frames.len() != fixture.frames {
        return Err(format!(
            "{} frames came out instead of {}",
            frames.len(),
            fixture.frames
        ));
    }
    if let Some(pair) = frames.windows(2).find(|pair| pair[0].0 >= pair[1].0) {
        return Err(format!(
            "a frame with pts {} came after one with pts {}",
            pair[1].0, pair[0].0
        ));
    }
    let sent: BTreeSet<i64> = fixture.packets.iter().map(|packet| packet.pts_us).collect();
    let decoded: BTreeSet<i64> = frames.iter().map(|(pts_us, _)| *pts_us).collect();
    if let Some(missing) = sent.difference(&decoded).next() {
        return Err(format!("no frame carries the input pts {missing}"));
    }
    if let Some(extra) = decoded.difference(&sent).next() {
        return Err(format!("a frame carries pts {extra}, which no input had"));
    }
    let VideoCheck::Hashes {
        frames: expected,
        stream: expected_stream,
    } = &video.check
    else {
        return Ok(());
    };
    for (index, ((_, hash), expected)) in frames.iter().zip(expected).enumerate() {
        if hash != expected {
            return Err(format!(
                "frame {index} has the NV12 SHA-256 {hash}, not {expected}"
            ));
        }
    }
    let stream = fixture::hex(stream.finish().as_ref());
    if stream != *expected_stream {
        return Err(format!(
            "the NV12 stream has the SHA-256 {stream}, not {expected_stream}"
        ));
    }
    Ok(())
}

fn decode_video_buffers(fixture: &Fixture, video: &VideoTrack) -> Result<FrameTimes, String> {
    let hashed = matches!(video.check, VideoCheck::Hashes { .. });
    let decoder = Decoder::configured(fixture, std::ptr::null_mut())?;
    let mut layout = None;
    let mut frames = Vec::new();
    let mut stream = Context::new(&SHA256);
    let mut timer = FrameTimer::start();
    drive(fixture, &decoder, |event| match event {
        Event::FormatChanged if layout.is_none() => {
            layout = Some(video_output(
                &decoder.output_format()?,
                video,
                COLOR_FORMAT_YUV420_SEMI_PLANAR,
            )?);
            Ok(())
        }
        Event::FormatChanged => Err("the output format changed more than once".to_owned()),
        Event::Frame { index, info } => {
            let layout = layout.ok_or("a frame came out before the output format")?;
            let hash = decoder.take_output(index, &info, |bytes| {
                if !hashed {
                    return Ok(String::new());
                }
                let nv12 = buffer_nv12(bytes, video, layout)?;
                stream.update(&nv12);
                Ok(fixture::sha256_hex(&nv12))
            })?;
            timer.frame();
            frames.push((info.presentation_time_us, hash));
            Ok(())
        }
    })?;
    check_video_frames(fixture, video, &frames, stream)?;
    Ok(timer.finish())
}

#[derive(Default)]
struct Announcements {
    count: Mutex<usize>,
    changed: Condvar,
}

unsafe extern "C" fn announce(context: *mut c_void, _reader: *mut c_void) {
    let announcements = unsafe { &*context.cast::<Announcements>() };
    *announcements
        .count
        .lock()
        .unwrap_or_else(PoisonError::into_inner) += 1;
    announcements.changed.notify_all();
}

struct Reader {
    handle: *mut c_void,
    announcements: Box<Announcements>,
}

impl Reader {
    fn new(video: &VideoTrack) -> Result<Self, String> {
        let mut handle = std::ptr::null_mut();
        status("AImageReader_new", unsafe {
            image_reader::eclipse_aimagereader_new(
                c_int_of("width", video.width)?,
                c_int_of("height", video.height)?,
                AIMAGE_FORMAT_YUV_420_888,
                READER_MAX_IMAGES,
                &mut handle,
            )
        })?;
        let reader = Self {
            handle,
            announcements: Box::default(),
        };
        let mut listener = ImageListenerDesc {
            context: std::ptr::from_ref::<Announcements>(&reader.announcements)
                .cast_mut()
                .cast(),
            on_image_available: Some(announce),
        };
        status("AImageReader_setImageListener", unsafe {
            image_reader::eclipse_aimagereader_setimagelistener(
                reader.handle,
                (&raw mut listener).cast(),
            )
        })?;
        Ok(reader)
    }

    fn window(&self) -> Result<*mut c_void, String> {
        let mut window = std::ptr::null_mut();
        status("AImageReader_getWindow", unsafe {
            image_reader::eclipse_aimagereader_getwindow(self.handle, &mut window)
        })?;
        Ok(window)
    }

    fn wait_for(&self, count: usize) -> Result<(), String> {
        let announced = self
            .announcements
            .count
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (announced, waited) = self
            .announcements
            .changed
            .wait_timeout_while(announced, LISTENER_WAIT, |announced| *announced < count)
            .unwrap_or_else(PoisonError::into_inner);
        if waited.timed_out() {
            return Err(format!(
                "the image listener announced {} of {count} frames",
                *announced
            ));
        }
        Ok(())
    }

    fn acquire(&self) -> Result<Image, String> {
        let mut image = std::ptr::null_mut();
        status("AImageReader_acquireLatestImage", unsafe {
            image_reader::eclipse_aimagereader_acquirelatestimage(self.handle, &mut image)
        })?;
        Ok(Image(image))
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        unsafe { image_reader::eclipse_aimagereader_delete(self.handle) };
    }
}

struct Plane<'image> {
    bytes: &'image [u8],
    row_stride: usize,
    pixel_stride: usize,
}

impl Plane<'_> {
    fn bytes(&self, start: usize, len: usize) -> Result<&[u8], String> {
        self.bytes.get(start..start + len).ok_or_else(|| {
            format!(
                "a {}-byte plane ends before byte {}",
                self.bytes.len(),
                start + len
            )
        })
    }
}

struct Image(*mut c_void);

impl Image {
    fn format(&self) -> Result<i32, String> {
        let mut format = 0;
        status("AImage_getFormat", unsafe {
            image_reader::eclipse_aimage_getformat(self.0, &mut format)
        })?;
        Ok(format)
    }

    fn crop(&self) -> Result<[i32; 4], String> {
        let mut rect = [0; 4];
        status("AImage_getCropRect", unsafe {
            image_reader::eclipse_aimage_getcroprect(self.0, &mut rect)
        })?;
        Ok(rect)
    }

    fn timestamp_ns(&self) -> Result<i64, String> {
        let mut timestamp = 0;
        status("AImage_getTimestamp", unsafe {
            image_reader::eclipse_aimage_gettimestamp(self.0, &mut timestamp)
        })?;
        Ok(timestamp)
    }

    fn plane(&self, plane: c_int) -> Result<Plane<'_>, String> {
        let (mut data, mut len, mut row_stride, mut pixel_stride) = (std::ptr::null_mut(), 0, 0, 0);
        status("AImage_getPlaneData", unsafe {
            image_reader::eclipse_aimage_getplanedata(self.0, plane, &mut data, &mut len)
        })?;
        status("AImage_getPlaneRowStride", unsafe {
            image_reader::eclipse_aimage_getplanerowstride(self.0, plane, &mut row_stride)
        })?;
        status("AImage_getPlanePixelStride", unsafe {
            image_reader::eclipse_aimage_getplanepixelstride(self.0, plane, &mut pixel_stride)
        })?;
        let (Ok(len), Ok(row_stride), Ok(pixel_stride)) = (
            usize::try_from(len),
            usize::try_from(row_stride),
            usize::try_from(pixel_stride),
        ) else {
            return Err(format!("plane {plane} reports a negative size"));
        };
        if data.is_null() {
            return Err(format!("plane {plane} has no data"));
        }
        Ok(Plane {
            bytes: unsafe { std::slice::from_raw_parts(data, len) },
            row_stride,
            pixel_stride,
        })
    }

    fn nv12(&self, video: &VideoTrack) -> Result<Vec<u8>, String> {
        let (chroma_width, chroma_height) = chroma_size(video);
        let [luma, u, v] = [0, 1, 2].map(|plane| self.plane(plane));
        let (luma, u, v) = (luma?, u?, v?);
        let strides = [luma.pixel_stride, u.pixel_stride, v.pixel_stride];
        if strides != [1, 2, 2] {
            return Err(format!("the pixel strides are {strides:?}, not [1, 2, 2]"));
        }
        let mut packed =
            Vec::with_capacity(video.width * video.height + 2 * chroma_width * chroma_height);
        for row in 0..video.height {
            packed.extend_from_slice(luma.bytes(row * luma.row_stride, video.width)?);
        }
        for row in 0..chroma_height {
            let u = u.bytes(row * u.row_stride, 2 * chroma_width - 1)?;
            let v = v.bytes(row * v.row_stride, 2 * chroma_width - 1)?;
            for column in 0..chroma_width {
                packed.extend_from_slice(&[u[2 * column], v[2 * column]]);
            }
        }
        Ok(packed)
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        unsafe { image_reader::eclipse_aimage_delete(self.0) };
    }
}

fn check_image(image: &Image, video: &VideoTrack, pts_us: i64) -> Result<(), String> {
    let format = image.format()?;
    if format != AIMAGE_FORMAT_YUV_420_888 {
        return Err(format!("an image has the format {format:#x}"));
    }
    let crop = image.crop()?;
    let full = [
        0,
        0,
        c_int_of("width", video.width)?,
        c_int_of("height", video.height)?,
    ];
    if crop != full {
        return Err(format!("an image is cropped to {crop:?}, not {full:?}"));
    }
    let timestamp = image.timestamp_ns()?;
    if timestamp != pts_us.saturating_mul(1000) {
        return Err(format!(
            "the image of pts {pts_us} µs has the timestamp {timestamp} ns"
        ));
    }
    Ok(())
}

fn decode_video_reader(fixture: &Fixture, video: &VideoTrack) -> Result<(), String> {
    let reader = Reader::new(video)?;
    let decoder = Decoder::configured(fixture, reader.window()?)?;
    let mut changed = false;
    let mut frames = Vec::new();
    let mut stream = Context::new(&SHA256);
    drive(fixture, &decoder, |event| match event {
        Event::FormatChanged if !changed => {
            video_output(&decoder.output_format()?, video, COLOR_FORMAT_SURFACE)?;
            changed = true;
            Ok(())
        }
        Event::FormatChanged => Err("the output format changed more than once".to_owned()),
        Event::Frame { index, info } => {
            if !changed {
                return Err("a frame came out before the output format".to_owned());
            }
            decoder.release(index, true)?;
            reader.wait_for(frames.len() + 1)?;
            let image = reader.acquire()?;
            check_image(&image, video, info.presentation_time_us)?;
            let nv12 = image.nv12(video)?;
            stream.update(&nv12);
            frames.push((info.presentation_time_us, fixture::sha256_hex(&nv12)));
            Ok(())
        }
    })?;
    check_video_frames(fixture, video, &frames, stream)
}

fn audio_output(format: &Format, audio: &AudioTrack) -> Result<(), String> {
    format.expect(
        KEY_SAMPLE_RATE,
        c_int_of("sample rate", audio.sample_rate as usize)?,
    )?;
    format.expect(
        KEY_CHANNEL_COUNT,
        c_int_of("channel count", audio.channels)?,
    )?;
    format.expect(KEY_PCM_ENCODING, PCM_ENCODING_16BIT)
}

fn decode_audio(fixture: &Fixture, audio: &AudioTrack) -> Result<(), String> {
    let frame_bytes = audio.samples_per_frame * audio.channels * size_of::<i16>();
    let decoder = Decoder::configured(fixture, std::ptr::null_mut())?;
    let mut changed = false;
    let mut pts = Vec::new();
    let mut channels = vec![Vec::new(); audio.channels];
    drive(fixture, &decoder, |event| match event {
        Event::FormatChanged if !changed => {
            audio_output(&decoder.output_format()?, audio)?;
            changed = true;
            Ok(())
        }
        Event::FormatChanged => Err("the output format changed more than once".to_owned()),
        Event::Frame { index, info } => {
            if !changed {
                return Err("a frame came out before the output format".to_owned());
            }
            decoder.take_output(index, &info, |bytes| {
                if bytes.len() != frame_bytes {
                    return Err(format!(
                        "an output holds {} bytes, not the {frame_bytes} of {} samples in {} \
                         channels",
                        bytes.len(),
                        audio.samples_per_frame,
                        audio.channels
                    ));
                }
                for (index, sample) in bytes.as_chunks::<2>().0.iter().enumerate() {
                    let sample = f32::from(i16::from_ne_bytes(*sample)) / 32768.0;
                    channels[index % audio.channels].push(sample);
                }
                Ok(())
            })?;
            pts.push(info.presentation_time_us);
            Ok(())
        }
    })?;
    if pts.len() != fixture.frames {
        return Err(format!(
            "{} frames came out instead of {}",
            pts.len(),
            fixture.frames
        ));
    }
    let sent = fixture.packets.iter().map(|packet| packet.pts_us);
    if let Some((index, (decoded, sent))) = pts
        .iter()
        .zip(sent)
        .enumerate()
        .find(|(_, (decoded, sent))| **decoded != *sent)
    {
        return Err(format!(
            "frame {index} carries pts {decoded}, not its input's {sent}"
        ));
    }
    audio.check_tone(&channels)
}

#[cfg(test)]
mod tests;
