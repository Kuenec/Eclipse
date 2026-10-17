use std::collections::BTreeSet;
use std::ops::Range;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use super::*;
use crate::loader::log_capture::formatted_log;
use crate::loader::mediacodec::fixture::{sha256_hex, Fixture, Track};
use crate::loader::native_provider::EclipseNativeProvider;
use crate::loader::resolve::SymbolProvider;

type CreateFn = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type ConfigureFn =
    unsafe extern "C" fn(*mut c_void, *const c_void, *mut c_void, *mut c_void, u32) -> MediaStatus;
type CodecCallFn = unsafe extern "C" fn(*mut c_void) -> MediaStatus;
type DequeueInputFn = unsafe extern "C" fn(*mut c_void, i64) -> isize;
type BufferFn = unsafe extern "C" fn(*mut c_void, usize, *mut usize) -> *mut u8;
type QueueInputFn =
    unsafe extern "C" fn(*mut c_void, usize, libc::off_t, usize, u64, u32) -> MediaStatus;
type DequeueOutputFn = unsafe extern "C" fn(*mut c_void, *mut BufferInfo, i64) -> isize;
type ReleaseOutputFn = unsafe extern "C" fn(*mut c_void, usize, bool) -> MediaStatus;
type OutputFormatFn = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type InputSurfaceFn = unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> MediaStatus;
type FormatNewFn = unsafe extern "C" fn() -> *mut c_void;
type SetInt32Fn = unsafe extern "C" fn(*mut c_void, *const c_char, i32);
type SetBufferFn = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_void, usize);
type GetInt32Fn = unsafe extern "C" fn(*mut c_void, *const c_char, *mut i32) -> bool;
type ToStringFn = unsafe extern "C" fn(*mut c_void) -> *const c_char;

struct Ndk {
    create: CreateFn,
    configure: ConfigureFn,
    start: CodecCallFn,
    stop: CodecCallFn,
    flush: CodecCallFn,
    delete: CodecCallFn,
    dequeue_input: DequeueInputFn,
    input_buffer: BufferFn,
    queue_input: QueueInputFn,
    dequeue_output: DequeueOutputFn,
    output_buffer: BufferFn,
    release_output: ReleaseOutputFn,
    output_format: OutputFormatFn,
    create_input_surface: InputSurfaceFn,
    signal_end_of_input: CodecCallFn,
    format_new: FormatNewFn,
    format_delete: CodecCallFn,
    set_int32: SetInt32Fn,
    set_buffer: SetBufferFn,
    get_int32: GetInt32Fn,
    to_string: ToStringFn,
}

pub(in crate::loader::mediacodec) fn native<F: Copy>(
    provider: &EclipseNativeProvider,
    name: &str,
) -> F {
    let addr = provider
        .resolve(name)
        .unwrap_or_else(|| panic!("{name} must be an Eclipse native"))
        .addr;
    assert_eq!(size_of::<F>(), size_of::<u64>());
    unsafe { std::mem::transmute_copy::<u64, F>(&addr) }
}

fn ndk() -> &'static Ndk {
    static NDK: OnceLock<Ndk> = OnceLock::new();
    NDK.get_or_init(|| {
        let provider = EclipseNativeProvider::with_bionic_natives();
        Ndk {
            create: native(&provider, "AMediaCodec_createDecoderByType"),
            configure: native(&provider, "AMediaCodec_configure"),
            start: native(&provider, "AMediaCodec_start"),
            stop: native(&provider, "AMediaCodec_stop"),
            flush: native(&provider, "AMediaCodec_flush"),
            delete: native(&provider, "AMediaCodec_delete"),
            dequeue_input: native(&provider, "AMediaCodec_dequeueInputBuffer"),
            input_buffer: native(&provider, "AMediaCodec_getInputBuffer"),
            queue_input: native(&provider, "AMediaCodec_queueInputBuffer"),
            dequeue_output: native(&provider, "AMediaCodec_dequeueOutputBuffer"),
            output_buffer: native(&provider, "AMediaCodec_getOutputBuffer"),
            release_output: native(&provider, "AMediaCodec_releaseOutputBuffer"),
            output_format: native(&provider, "AMediaCodec_getOutputFormat"),
            create_input_surface: native(&provider, "AMediaCodec_createInputSurface"),
            signal_end_of_input: native(&provider, "AMediaCodec_signalEndOfInputStream"),
            format_new: native(&provider, "AMediaFormat_new"),
            format_delete: native(&provider, "AMediaFormat_delete"),
            set_int32: native(&provider, "AMediaFormat_setInt32"),
            set_buffer: native(&provider, "AMediaFormat_setBuffer"),
            get_int32: native(&provider, "AMediaFormat_getInt32"),
            to_string: native(&provider, "AMediaFormat_toString"),
        }
    })
}

pub(in crate::loader::mediacodec) fn decodable(test: &str, codec: Codec) -> bool {
    let backend = avcodec::backend();
    match backend.library() {
        Err(reason) => eprintln!("{test}: {reason}; skipping"),
        Ok(library) if !backend.decoders().contains(&codec) => eprintln!(
            "{test}: {} has no {codec:?} decoder; skipping",
            library.soname()
        ),
        Ok(_) => return true,
    }
    false
}

fn missing_backend() -> &'static Backend {
    static MISSING: OnceLock<Backend> = OnceLock::new();
    MISSING.get_or_init(|| Backend::load(&[]))
}

struct TestFormat(*mut c_void);

impl TestFormat {
    fn new() -> Self {
        let format = unsafe { (ndk().format_new)() };
        assert!(!format.is_null(), "AMediaFormat_new returns a format");
        Self(format)
    }

    fn of(fixture: &Fixture) -> Self {
        let format = Self::new();
        let sizes = match &fixture.track {
            Track::Video(video) => [(KEY_WIDTH, video.width), (KEY_HEIGHT, video.height)],
            Track::Audio(audio) => [
                (KEY_SAMPLE_RATE, audio.sample_rate as usize),
                (KEY_CHANNEL_COUNT, audio.channels),
            ],
        };
        for (key, value) in sizes {
            format.set_int32(key, i32::try_from(value).expect("fixture values fit"));
        }
        for (key, bytes) in [(KEY_CSD_0, fixture.csd_0()), (KEY_CSD_1, fixture.csd_1())] {
            if !bytes.is_empty() {
                format.set_buffer(key, bytes);
            }
        }
        format
    }

    fn set_int32(&self, key: &CStr, value: i32) {
        unsafe { (ndk().set_int32)(self.0, key.as_ptr(), value) }
    }

    fn set_buffer(&self, key: &CStr, bytes: &[u8]) {
        unsafe { (ndk().set_buffer)(self.0, key.as_ptr(), bytes.as_ptr().cast(), bytes.len()) }
    }

    fn int32(&self, key: &CStr) -> Option<i32> {
        let mut value = i32::MIN;
        unsafe { (ndk().get_int32)(self.0, key.as_ptr(), &mut value) }.then_some(value)
    }

    fn description(&self) -> String {
        let text = unsafe { (ndk().to_string)(self.0) };
        unsafe { CStr::from_ptr(text) }
            .to_string_lossy()
            .into_owned()
    }
}

impl Drop for TestFormat {
    fn drop(&mut self) {
        unsafe { (ndk().format_delete)(self.0) };
    }
}

enum Dequeue {
    TryAgain,
    FormatChanged,
    Buffer { index: usize, info: BufferInfo },
}

#[derive(Debug)]
enum Event {
    FormatChanged,
    Buffer {
        pts_us: i64,
        flags: u32,
        bytes: Vec<u8>,
    },
}

impl Event {
    fn ends_stream(&self) -> bool {
        match self {
            Event::Buffer { flags, .. } => flags & AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM != 0,
            Event::FormatChanged => false,
        }
    }
}

struct TestCodec(*mut c_void);

impl TestCodec {
    fn create(mime: &CStr) -> Self {
        let codec = unsafe { (ndk().create)(mime.as_ptr()) };
        assert!(
            !codec.is_null(),
            "createDecoderByType({mime:?}) returns a codec"
        );
        Self(codec)
    }

    fn configure(&self, format: &TestFormat) -> MediaStatus {
        self.configure_to(format, std::ptr::null_mut())
    }

    fn configure_to(&self, format: &TestFormat, surface: *mut c_void) -> MediaStatus {
        unsafe { (ndk().configure)(self.0, format.0, surface, std::ptr::null_mut(), 0) }
    }

    fn started(fixture: &Fixture) -> Self {
        let codec = Self::create(&fixture.mime);
        assert_eq!(codec.configure(&TestFormat::of(fixture)), AMEDIA_OK);
        assert_eq!(codec.start(), AMEDIA_OK);
        codec
    }

    fn start(&self) -> MediaStatus {
        unsafe { (ndk().start)(self.0) }
    }

    fn stop(&self) -> MediaStatus {
        unsafe { (ndk().stop)(self.0) }
    }

    fn flush(&self) -> MediaStatus {
        unsafe { (ndk().flush)(self.0) }
    }

    fn dequeue_input(&self, timeout_us: i64) -> isize {
        unsafe { (ndk().dequeue_input)(self.0, timeout_us) }
    }

    fn queue_input(&self, index: usize, size: usize, pts_us: i64, flags: u32) -> MediaStatus {
        unsafe { (ndk().queue_input)(self.0, index, 0, size, pts_us as u64, flags) }
    }

    fn feed(&self, bytes: &[u8], pts_us: i64, flags: u32) -> bool {
        let index = self.dequeue_input(0);
        if index == AMEDIACODEC_INFO_TRY_AGAIN_LATER {
            return false;
        }
        let index = usize::try_from(index)
            .unwrap_or_else(|_| panic!("dequeueInputBuffer failed with {index}"));
        let mut capacity = 0;
        let buffer = unsafe { (ndk().input_buffer)(self.0, index, &mut capacity) };
        assert!(!buffer.is_null(), "a dequeued input has a buffer");
        assert!(capacity >= bytes.len(), "{capacity} < {}", bytes.len());
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, bytes.len()) };
        assert_eq!(
            self.queue_input(index, bytes.len(), pts_us, flags),
            AMEDIA_OK
        );
        true
    }

    fn dequeue_output(&self, timeout_us: i64) -> Dequeue {
        let mut info = BufferInfo::default();
        match unsafe { (ndk().dequeue_output)(self.0, &mut info, timeout_us) } {
            AMEDIACODEC_INFO_TRY_AGAIN_LATER => Dequeue::TryAgain,
            AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED => Dequeue::FormatChanged,
            index => Dequeue::Buffer {
                index: usize::try_from(index)
                    .unwrap_or_else(|_| panic!("dequeueOutputBuffer failed with {index}")),
                info,
            },
        }
    }

    fn read_output(&self, index: usize, info: BufferInfo) -> Event {
        let mut capacity = 0;
        let buffer = unsafe { (ndk().output_buffer)(self.0, index, &mut capacity) };
        assert!(!buffer.is_null(), "a dequeued output has a buffer");
        assert_eq!(info.offset, 0);
        let size = usize::try_from(info.size).expect("a non-negative output size");
        assert!(size <= capacity, "{size} > {capacity}");
        Event::Buffer {
            pts_us: info.presentation_time_us,
            flags: info.flags,
            bytes: unsafe { std::slice::from_raw_parts(buffer, size) }.to_vec(),
        }
    }

    fn release_output(&self, index: usize) -> MediaStatus {
        self.release(index, false)
    }

    fn release(&self, index: usize, render: bool) -> MediaStatus {
        unsafe { (ndk().release_output)(self.0, index, render) }
    }

    fn next_event(&self) -> Option<Event> {
        match self.dequeue_output(0) {
            Dequeue::TryAgain => None,
            Dequeue::FormatChanged => Some(Event::FormatChanged),
            Dequeue::Buffer { index, info } => {
                let event = self.read_output(index, info);
                assert_eq!(self.release_output(index), AMEDIA_OK);
                Some(event)
            }
        }
    }

    fn output_format(&self) -> TestFormat {
        let format = unsafe { (ndk().output_format)(self.0) };
        assert!(!format.is_null(), "getOutputFormat returns a format");
        TestFormat(format)
    }

    fn decode(&self, fixture: &Fixture, packets: Range<usize>) -> Vec<Event> {
        let mut events = Vec::new();
        let mut next = packets.start;
        let mut ended = false;
        for _ in 0..1000 {
            while !ended {
                let fed = if next < packets.end {
                    let packet = &fixture.packets[next];
                    self.feed(fixture.packet(next), packet.pts_us, packet.flags)
                } else {
                    self.feed(&[], 0, AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM)
                };
                if !fed {
                    break;
                }
                if next < packets.end {
                    next += 1;
                } else {
                    ended = true;
                }
            }
            if let Some(event) = self.next_event() {
                let last = event.ends_stream();
                events.push(event);
                if last {
                    return events;
                }
            }
        }
        panic!("the decoder never ended the stream after {events:?}");
    }
}

impl Drop for TestCodec {
    fn drop(&mut self) {
        unsafe { (ndk().delete)(self.0) };
    }
}

pub(in crate::loader::mediacodec) struct RenderingDecoder {
    codec: TestCodec,
    fixture: Fixture,
    next: usize,
    ended: bool,
}

impl RenderingDecoder {
    pub(in crate::loader::mediacodec) fn start(name: &str, window: *mut c_void) -> Self {
        let fixture = Fixture::load(name);
        let codec = TestCodec::create(&fixture.mime);
        assert_eq!(
            codec.configure_to(&TestFormat::of(&fixture), window),
            AMEDIA_OK
        );
        assert_eq!(codec.start(), AMEDIA_OK);
        Self {
            codec,
            fixture,
            next: 0,
            ended: false,
        }
    }

    pub(in crate::loader::mediacodec) fn fixture(&self) -> &Fixture {
        &self.fixture
    }

    fn feed_available(&mut self) {
        while !self.ended {
            let fed = match self.fixture.packets.get(self.next) {
                Some(packet) => {
                    self.codec
                        .feed(self.fixture.packet(self.next), packet.pts_us, packet.flags)
                }
                None => self
                    .codec
                    .feed(&[], 0, AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM),
            };
            if !fed {
                return;
            }
            if self.next < self.fixture.packets.len() {
                self.next += 1;
            } else {
                self.ended = true;
            }
        }
    }

    pub(in crate::loader::mediacodec) fn render_next(&mut self) -> Option<(i64, MediaStatus)> {
        for _ in 0..1000 {
            self.feed_available();
            match self.codec.dequeue_output(0) {
                Dequeue::TryAgain => {}
                Dequeue::FormatChanged => assert_eq!(
                    self.codec.output_format().int32(KEY_COLOR_FORMAT),
                    Some(COLOR_FORMAT_SURFACE),
                    "reader outputs report COLOR_FormatSurface"
                ),
                Dequeue::Buffer { index, info } => {
                    let mut size = 0;
                    let buffer = unsafe { (ndk().output_buffer)(self.codec.0, index, &mut size) };
                    assert!(buffer.is_null(), "reader outputs have no buffer");
                    if info.flags & AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM != 0 {
                        assert_eq!(self.codec.release(index, false), AMEDIA_OK);
                        return None;
                    }
                    assert!(info.size > 0, "a frame for a reader reports its size");
                    let status = self.codec.release(index, true);
                    return Some((info.presentation_time_us, status));
                }
            }
        }
        panic!("the decoder rendered nothing after 1000 dequeues");
    }
}

fn frames(events: &[Event]) -> Vec<(i64, &[u8])> {
    let Some((Event::Buffer { flags, bytes, .. }, body)) = events.split_last() else {
        panic!("the stream ends with a buffer: {events:?}");
    };
    assert_eq!(
        flags & AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM,
        AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM,
        "the last output carries flag 4"
    );
    assert!(bytes.is_empty(), "the end-of-stream output is empty");
    body.iter()
        .filter_map(|event| match event {
            Event::FormatChanged => None,
            Event::Buffer {
                pts_us,
                flags,
                bytes,
            } => {
                assert_eq!(*flags, 0, "decoded frames carry no flags");
                Some((*pts_us, bytes.as_slice()))
            }
        })
        .collect()
}

fn format_changes(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, Event::FormatChanged))
        .count()
}

fn assert_frames_match(name: &str, fixture: &Fixture, frames: &[(i64, &[u8])]) {
    assert_eq!(frames.len(), fixture.frames, "{name} frame count");
    let pts: Vec<i64> = frames.iter().map(|(pts, _)| *pts).collect();
    assert!(
        pts.windows(2).all(|pair| pair[0] < pair[1]),
        "{name} pts must ascend: {pts:?}"
    );
    let sent: BTreeSet<i64> = fixture.packets.iter().map(|packet| packet.pts_us).collect();
    assert_eq!(
        pts.into_iter().collect::<BTreeSet<_>>(),
        sent,
        "{name} pts set"
    );
    let (frame_hashes, _) = fixture.hashes();
    for (index, (_, bytes)) in frames.iter().enumerate() {
        assert_eq!(
            sha256_hex(bytes),
            frame_hashes[index],
            "{name} frame {index} NV12 hash"
        );
    }
}

fn assert_video_output_format(codec: &TestCodec, fixture: &Fixture) {
    let format = codec.output_format();
    let width = i32::try_from(fixture.video().width).expect("a small width");
    let height = i32::try_from(fixture.video().height).expect("a small height");
    for (key, value) in [
        (KEY_WIDTH, width),
        (KEY_HEIGHT, height),
        (KEY_STRIDE, width),
        (KEY_SLICE_HEIGHT, height),
        (KEY_COLOR_FORMAT, COLOR_FORMAT_YUV420_SEMI_PLANAR),
        (KEY_CROP_LEFT, 0),
        (KEY_CROP_TOP, 0),
        (KEY_CROP_RIGHT, width - 1),
        (KEY_CROP_BOTTOM, height - 1),
    ] {
        assert_eq!(format.int32(key), Some(value), "output {key:?}");
    }
    let description = format.description();
    assert!(
        description.contains("mime: string(video/raw)"),
        "{description}"
    );
}

fn check_video_fixture(test: &str, name: &str) {
    let fixture = Fixture::load(name);
    if !decodable(test, fixture.codec()) {
        return;
    }
    let codec = TestCodec::started(&fixture);
    let events = codec.decode(&fixture, 0..fixture.packets.len());
    assert!(
        matches!(events.first(), Some(Event::FormatChanged)),
        "the first output is a format change: {:?}",
        events.first()
    );
    assert_eq!(format_changes(&events), 1, "{name} keeps one format");
    assert_video_output_format(&codec, &fixture);
    assert_frames_match(name, &fixture, &frames(&events));
}

#[test]
fn buffer_info_matches_the_ndk_layout_and_flags() {
    assert_eq!(AMEDIACODEC_BUFFER_FLAG_KEY_FRAME, 1);
    assert_eq!(AMEDIACODEC_BUFFER_FLAG_CODEC_CONFIG, 2);
    assert_eq!(AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM, 4);
    assert_eq!(AMEDIACODEC_CONFIGURE_FLAG_ENCODE, 1);
    assert_eq!(AMEDIACODEC_INFO_TRY_AGAIN_LATER, -1);
    assert_eq!(AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED, -2);
}

#[test]
fn h264_with_b_frames_decodes_to_nv12_buffers() {
    check_video_fixture("h264_with_b_frames_decodes_to_nv12_buffers", "h264-bframes");
}

#[test]
fn hevc_decodes_to_nv12_buffers() {
    check_video_fixture("hevc_decodes_to_nv12_buffers", "hevc");
}

#[test]
fn aac_decodes_to_interleaved_pcm_in_the_configured_encoding() {
    let fixture = Fixture::load("aac-lc");
    if !decodable(
        "aac_decodes_to_interleaved_pcm_in_the_configured_encoding",
        Codec::Aac,
    ) {
        return;
    }
    let channels = fixture.audio().channels;
    let rate = i32::try_from(fixture.audio().sample_rate).expect("a rate");
    for (encoding, sample_bytes) in [(PCM_ENCODING_16BIT, 2), (PCM_ENCODING_FLOAT, 4)] {
        let codec = TestCodec::create(c"audio/mp4a-latm");
        let format = TestFormat::of(&fixture);
        format.set_int32(KEY_PCM_ENCODING, encoding);
        assert_eq!(codec.configure(&format), AMEDIA_OK);
        assert_eq!(codec.start(), AMEDIA_OK);
        let events = codec.decode(&fixture, 0..fixture.packets.len());
        assert!(matches!(events.first(), Some(Event::FormatChanged)));
        assert_eq!(format_changes(&events), 1);

        let output = codec.output_format();
        assert_eq!(output.int32(KEY_SAMPLE_RATE), Some(rate));
        assert_eq!(
            output.int32(KEY_CHANNEL_COUNT),
            Some(i32::try_from(channels).expect("a channel count"))
        );
        assert_eq!(output.int32(KEY_PCM_ENCODING), Some(encoding));
        assert!(output.description().contains("mime: string(audio/raw)"));

        let frames = frames(&events);
        assert_eq!(frames.len(), fixture.frames);
        let pts: Vec<i64> = frames.iter().map(|(pts, _)| *pts).collect();
        let sent: Vec<i64> = fixture.packets.iter().map(|packet| packet.pts_us).collect();
        assert_eq!(pts, sent, "AAC frames keep their input pts");
        for (_, bytes) in &frames {
            assert_eq!(
                bytes.len(),
                fixture.audio().samples_per_frame * channels * sample_bytes
            );
        }
    }
}

#[test]
fn flush_after_four_frames_restarts_from_the_keyframe() {
    let fixture = Fixture::load("h264-bframes");
    if !decodable(
        "flush_after_four_frames_restarts_from_the_keyframe",
        Codec::H264,
    ) {
        return;
    }
    let codec = TestCodec::started(&fixture);
    let mut next = 0;
    let mut decoded = 0;
    let mut held = None;
    for _ in 0..1000 {
        while next < fixture.packets.len() {
            let packet = &fixture.packets[next];
            if !codec.feed(fixture.packet(next), packet.pts_us, packet.flags) {
                break;
            }
            next += 1;
        }
        match codec.dequeue_output(0) {
            Dequeue::Buffer { index, .. } if decoded == 3 => {
                held = Some(index);
                break;
            }
            Dequeue::Buffer { index, .. } => {
                decoded += 1;
                assert_eq!(codec.release_output(index), AMEDIA_OK);
            }
            Dequeue::TryAgain | Dequeue::FormatChanged => {}
        }
    }
    let held = held.expect("the fourth frame was dequeued");

    assert_eq!(codec.flush(), AMEDIA_OK);
    assert_eq!(
        codec.release_output(held),
        AMEDIA_ERROR_INVALID_PARAMETER,
        "flush invalidates outstanding output indices"
    );
    let events = codec.decode(&fixture, 0..fixture.packets.len());
    assert_eq!(
        format_changes(&events),
        0,
        "a flush keeps the output format"
    );
    assert_frames_match("h264-bframes after flush", &fixture, &frames(&events));
}

#[test]
fn codec_configuration_buffers_stand_in_for_csd_keys() {
    let fixture = Fixture::load("h264-bframes");
    if !decodable(
        "codec_configuration_buffers_stand_in_for_csd_keys",
        Codec::H264,
    ) {
        return;
    }
    let codec = TestCodec::create(c"video/avc");
    let format = TestFormat::new();
    format.set_int32(KEY_WIDTH, 64);
    format.set_int32(KEY_HEIGHT, 48);
    assert_eq!(codec.configure(&format), AMEDIA_OK);
    assert_eq!(codec.start(), AMEDIA_OK);
    for csd in [fixture.csd_0(), fixture.csd_1()] {
        assert!(codec.feed(csd, 0, AMEDIACODEC_BUFFER_FLAG_CODEC_CONFIG));
    }
    let events = codec.decode(&fixture, 0..fixture.packets.len());
    assert_frames_match("h264-bframes with in-band csd", &fixture, &frames(&events));
}

#[test]
fn unknown_mimes_and_unavailable_backends_get_no_decoder() {
    assert!(unsafe { (ndk().create)(c"video/x-unknown".as_ptr()) }.is_null());
    assert!(unsafe { (ndk().create)(std::ptr::null()) }.is_null());

    let codecs = Codecs::new();
    assert!(matches!(
        codecs.create_decoder(missing_backend, c"video/avc"),
        Err(Refusal::Unavailable(BackendUnavailable::NotFound { .. }))
    ));
    assert!(matches!(
        codecs.create_decoder(missing_backend, c"video/x-unknown"),
        Err(Refusal::UnknownMime)
    ));
}

#[test]
fn the_seventeenth_live_codec_is_refused() {
    let Some(&codec) = avcodec::backend().decoders().first() else {
        eprintln!("the_seventeenth_live_codec_is_refused: no FFmpeg decoders; skipping");
        return;
    };
    let mime = CString::new(codec.mime()).expect("a C MIME");
    let codecs = Codecs::new();
    let live: Vec<NdkHandle> = (0..MAX_LIVE_CODECS)
        .map(|_| {
            codecs
                .create_decoder(avcodec::backend, &mime)
                .expect("one of the first 16 codecs")
        })
        .collect();
    assert!(matches!(
        codecs.create_decoder(avcodec::backend, &mime),
        Err(Refusal::TooManyCodecs)
    ));
    codecs
        .delete(handle_to_ptr(live[0]))
        .expect("delete a live codec");
    let replacement = codecs
        .create_decoder(avcodec::backend, &mime)
        .expect("a deleted codec frees its place");
    for handle in live.into_iter().skip(1).chain([replacement]) {
        codecs.delete(handle_to_ptr(handle)).expect("delete");
    }
}

#[test]
fn configure_rejects_encoding_crypto_surfaces_and_unusable_sizes() {
    if !decodable(
        "configure_rejects_encoding_crypto_surfaces_and_unusable_sizes",
        Codec::H264,
    ) {
        return;
    }
    let codec = TestCodec::create(c"video/avc");
    let format = TestFormat::new();
    format.set_int32(KEY_WIDTH, 64);
    format.set_int32(KEY_HEIGHT, 48);
    let mut token = 0u8;
    let token: *mut c_void = (&raw mut token).cast();
    let null = std::ptr::null_mut();
    let configure = |surface, crypto, flags| unsafe {
        (ndk().configure)(codec.0, format.0, surface, crypto, flags)
    };
    assert_eq!(
        configure(null, null, AMEDIACODEC_CONFIGURE_FLAG_ENCODE),
        AMEDIA_ERROR_UNSUPPORTED
    );
    assert_eq!(configure(null, token, 0), AMEDIA_ERROR_UNSUPPORTED);
    assert_eq!(configure(token, null, 0), AMEDIA_ERROR_UNSUPPORTED);

    format.set_int32(KEY_HEIGHT, MAX_DIMENSION + 1);
    assert_eq!(configure(null, null, 0), AMEDIA_ERROR_UNSUPPORTED);
    format.set_int32(KEY_HEIGHT, -48);
    assert_eq!(configure(null, null, 0), AMEDIA_ERROR_INVALID_PARAMETER);
    format.set_int32(KEY_HEIGHT, 48);
    let too_large = i32::try_from(MAX_INPUT_CAPACITY + 1).expect("the bound fits an int");
    format.set_int32(KEY_MAX_INPUT_SIZE, too_large);
    assert_eq!(configure(null, null, 0), AMEDIA_ERROR_INVALID_PARAMETER);

    for (requested, capacity) in [(1, MIN_VIDEO_INPUT_CAPACITY), (3 << 20, 3 << 20)] {
        format.set_int32(KEY_MAX_INPUT_SIZE, requested);
        assert_eq!(configure(null, null, 0), AMEDIA_OK);
        assert_eq!(configure(null, null, 0), AMEDIA_ERROR_INVALID_OPERATION);
        assert_eq!(codec.start(), AMEDIA_OK);
        let index = usize::try_from(codec.dequeue_input(0)).expect("a free input");
        let mut size = 0;
        assert!(!unsafe { (ndk().input_buffer)(codec.0, index, &mut size) }.is_null());
        assert_eq!(size, capacity, "max-input-size {requested}");
        assert_eq!(codec.stop(), AMEDIA_OK);
    }
}

#[test]
fn lifecycle_calls_outside_their_state_are_invalid_operations() {
    let fixture = Fixture::load("aac-lc");
    if !decodable(
        "lifecycle_calls_outside_their_state_are_invalid_operations",
        Codec::Aac,
    ) {
        return;
    }
    let codec = TestCodec::create(c"audio/mp4a-latm");
    assert_eq!(codec.start(), AMEDIA_ERROR_INVALID_OPERATION);
    assert_eq!(codec.stop(), AMEDIA_ERROR_INVALID_OPERATION);
    assert_eq!(codec.flush(), AMEDIA_ERROR_INVALID_OPERATION);
    assert_eq!(
        codec.dequeue_input(0),
        AMEDIA_ERROR_INVALID_OPERATION as isize
    );
    assert!(unsafe { (ndk().output_format)(codec.0) }.is_null());

    let format = TestFormat::of(&fixture);
    assert_eq!(codec.configure(&format), AMEDIA_OK);
    assert_eq!(codec.flush(), AMEDIA_ERROR_INVALID_OPERATION);
    assert_eq!(codec.start(), AMEDIA_OK);
    assert_eq!(codec.start(), AMEDIA_ERROR_INVALID_OPERATION);
    assert!(codec.feed(fixture.packet(0), 0, 0));
    assert_eq!(codec.stop(), AMEDIA_OK);
    assert_eq!(
        codec.dequeue_input(0),
        AMEDIA_ERROR_INVALID_OPERATION as isize
    );

    assert_eq!(codec.configure(&format), AMEDIA_OK);
    assert_eq!(codec.start(), AMEDIA_OK);
    let events = codec.decode(&fixture, 0..fixture.packets.len());
    assert_eq!(
        frames(&events).len(),
        fixture.frames,
        "a stopped codec restarts"
    );
}

#[test]
fn an_end_of_stream_without_frames_ends_with_one_empty_buffer_and_refuses_more_input() {
    let fixture = Fixture::load("aac-lc");
    if !decodable(
        "an_end_of_stream_without_frames_ends_with_one_empty_buffer_and_refuses_more_input",
        Codec::Aac,
    ) {
        return;
    }
    let codec = TestCodec::started(&fixture);
    assert!(codec.feed(&[], 7, AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM));
    let index = usize::try_from(codec.dequeue_input(0)).expect("inputs stay dequeueable");
    assert_eq!(
        codec.queue_input(index, 0, 0, 0),
        AMEDIA_ERROR_INVALID_OPERATION
    );
    let event = (0..100)
        .find_map(|_| codec.next_event())
        .expect("an end-of-stream output");
    let Event::Buffer {
        pts_us,
        flags,
        bytes,
    } = event
    else {
        panic!("no frame means no format change: {event:?}");
    };
    assert_eq!(
        (pts_us, flags, bytes.len()),
        (7, AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM, 0)
    );
    assert!(codec.next_event().is_none(), "the stream ends once");
}

fn fill_until_inputs_back_up(codec: &TestCodec, fixture: &Fixture) -> usize {
    for queued in 0..64 {
        let packet = queued % fixture.packets.len();
        let pts_us = i64::try_from(queued).expect("a small count") * 21_333;
        if !codec.feed(fixture.packet(packet), pts_us, 1) {
            return queued;
        }
    }
    panic!("64 inputs went in without any output being drained");
}

fn release_one_output(codec: &TestCodec) {
    let index = (0..4)
        .find_map(|_| match codec.dequeue_output(0) {
            Dequeue::Buffer { index, .. } => Some(index),
            Dequeue::FormatChanged | Dequeue::TryAgain => None,
        })
        .expect("a decoded buffer is waiting");
    assert_eq!(codec.release_output(index), AMEDIA_OK);
}

#[test]
fn full_input_slots_make_a_zero_timeout_input_dequeue_try_again() {
    let fixture = Fixture::load("aac-lc");
    if !decodable(
        "full_input_slots_make_a_zero_timeout_input_dequeue_try_again",
        Codec::Aac,
    ) {
        return;
    }
    let codec = TestCodec::started(&fixture);
    let queued = fill_until_inputs_back_up(&codec, &fixture);
    assert!(
        queued >= INPUT_SLOTS + OUTPUT_SLOTS,
        "only {queued} inputs fit"
    );
    assert_eq!(codec.dequeue_input(0), AMEDIACODEC_INFO_TRY_AGAIN_LATER);

    let freed = (0..OUTPUT_SLOTS).find(|_| {
        release_one_output(&codec);
        codec.dequeue_input(0) >= 0
    });
    assert!(
        freed.is_some(),
        "released outputs let the decoder take input"
    );
}

#[test]
fn a_blocked_input_dequeue_waits_its_timeout_and_wakes_when_an_output_is_released() {
    let fixture = Fixture::load("aac-lc");
    if !decodable(
        "a_blocked_input_dequeue_waits_its_timeout_and_wakes_when_an_output_is_released",
        Codec::Aac,
    ) {
        return;
    }
    let codec = TestCodec::started(&fixture);
    fill_until_inputs_back_up(&codec, &fixture);

    let started = Instant::now();
    assert_eq!(
        codec.dequeue_input(20_000),
        AMEDIACODEC_INFO_TRY_AGAIN_LATER
    );
    assert!(started.elapsed() >= Duration::from_millis(20));

    let raw = codec.0 as usize;
    let waiter = std::thread::spawn(move || {
        let started = Instant::now();
        let index = unsafe { (ndk().dequeue_input)(raw as *mut c_void, 10_000_000) };
        (index, started.elapsed())
    });
    for _ in 0..OUTPUT_SLOTS {
        std::thread::sleep(Duration::from_millis(50));
        if waiter.is_finished() {
            break;
        }
        release_one_output(&codec);
    }
    let (index, waited) = waiter.join().expect("the waiting thread");
    assert!(index >= 0, "the waiter got an input: {index}");
    assert!(waited < Duration::from_secs(5), "woken after {waited:?}");
}

#[test]
fn stale_and_fabricated_codec_handles_are_invalid_objects() {
    let mut handles = vec![std::ptr::null_mut(), 0xdead_beef_usize as *mut c_void];
    if decodable(
        "stale_and_fabricated_codec_handles_are_invalid_objects",
        Codec::Aac,
    ) {
        let codec = TestCodec::create(c"audio/mp4a-latm");
        handles.push(codec.0);
    }
    let format = TestFormat::new();
    for handle in handles {
        let ndk = ndk();
        let mut info = BufferInfo::default();
        let mut size = 0;
        let null = std::ptr::null_mut();
        unsafe {
            assert_eq!(
                (ndk.configure)(handle, format.0, null, null, 0),
                AMEDIA_ERROR_INVALID_OBJECT
            );
            for call in [ndk.start, ndk.stop, ndk.flush, ndk.delete] {
                assert_eq!(call(handle), AMEDIA_ERROR_INVALID_OBJECT);
            }
            assert_eq!(
                (ndk.dequeue_input)(handle, 0),
                AMEDIA_ERROR_INVALID_OBJECT as isize
            );
            assert!((ndk.input_buffer)(handle, 0, &mut size).is_null());
            assert_eq!(
                (ndk.queue_input)(handle, 0, 0, 0, 0, 0),
                AMEDIA_ERROR_INVALID_OBJECT
            );
            assert_eq!(
                (ndk.dequeue_output)(handle, &mut info, 0),
                AMEDIA_ERROR_INVALID_OBJECT as isize
            );
            assert!((ndk.output_buffer)(handle, 0, &mut size).is_null());
            assert_eq!(
                (ndk.release_output)(handle, 0, false),
                AMEDIA_ERROR_INVALID_OBJECT
            );
            assert!((ndk.output_format)(handle).is_null());
            assert_eq!(
                (ndk.create_input_surface)(handle, &mut std::ptr::null_mut()),
                AMEDIA_ERROR_INVALID_OBJECT
            );
            assert_eq!(
                (ndk.signal_end_of_input)(handle),
                AMEDIA_ERROR_INVALID_OBJECT
            );
        }
        assert_eq!(size, 0, "failed buffer lookups leave the size alone");
    }
}

#[test]
fn decoders_have_no_input_surface_and_no_encoder_end_of_stream() {
    if !decodable(
        "decoders_have_no_input_surface_and_no_encoder_end_of_stream",
        Codec::Aac,
    ) {
        return;
    }
    let codec = TestCodec::create(c"audio/mp4a-latm");
    let mut surface = std::ptr::null_mut();
    assert_eq!(
        unsafe { (ndk().create_input_surface)(codec.0, &mut surface) },
        AMEDIA_ERROR_INVALID_OPERATION
    );
    assert!(surface.is_null());
    assert_eq!(
        unsafe { (ndk().signal_end_of_input)(codec.0) },
        AMEDIA_ERROR_INVALID_OPERATION
    );
}

#[test]
fn refusals_are_logged_at_info_once_per_mime() {
    let create = ndk().create;
    let log = formatted_log("mediandk=info", || {
        for _ in 0..2 {
            assert!(unsafe { create(c"video/x-eclipse-refused".as_ptr()) }.is_null());
        }
    });
    let lines: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("video/x-eclipse-refused"))
        .collect();
    assert_eq!(lines.len(), 1, "{log}");
    assert!(lines[0].contains("INFO mediandk:"), "{}", lines[0]);
    assert!(
        lines[0].contains("Eclipse has no decoder for this MIME type"),
        "{}",
        lines[0]
    );
}

#[test]
fn the_java_codec_list_advertises_the_decoder_limits() {
    let java =
        include_str!("../../../../tools/framework-overlay/src/android/media/MediaCodecInfo.java");
    assert!(java.contains(&format!(
        "private static final int MAX_SUPPORTED_INSTANCES = {MAX_LIVE_CODECS};"
    )));
    assert!(java.contains(&format!(
        "private static final int MAX_SIZE = {MAX_DIMENSION};"
    )));
}
