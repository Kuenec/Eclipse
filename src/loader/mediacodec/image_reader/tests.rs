use std::ffi::{c_char, CStr};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use super::*;
use crate::loader::mediacodec::avcodec::Codec;
use crate::loader::mediacodec::codec::tests::{decodable, native, RenderingDecoder};
use crate::loader::mediacodec::fixture::sha256_hex;
use crate::loader::native_provider::EclipseNativeProvider;

type NewFn = unsafe extern "C" fn(i32, i32, i32, i32, *mut *mut c_void) -> MediaStatus;
type NewWithUsageFn =
    unsafe extern "C" fn(i32, i32, i32, u64, i32, *mut *mut c_void) -> MediaStatus;
type ReaderOutFn = unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> MediaStatus;
type ListenerFn = unsafe extern "C" fn(*mut c_void, *mut ImageListenerDesc) -> MediaStatus;
type ReleaseFn = unsafe extern "C" fn(*mut c_void);
type ImageIntFn = unsafe extern "C" fn(*mut c_void, *mut i32) -> MediaStatus;
type CropFn = unsafe extern "C" fn(*mut c_void, *mut [i32; 4]) -> MediaStatus;
type TimestampFn = unsafe extern "C" fn(*mut c_void, *mut i64) -> MediaStatus;
type PlaneIntFn = unsafe extern "C" fn(*mut c_void, c_int, *mut i32) -> MediaStatus;
type PlaneDataFn =
    unsafe extern "C" fn(*mut c_void, c_int, *mut *mut u8, *mut c_int) -> MediaStatus;
type DeleteAsyncFn = unsafe extern "C" fn(*mut c_void, c_int);
type WindowQueryFn = unsafe extern "C" fn(*mut c_void) -> i32;
type DescribeFn = unsafe extern "C" fn(*const c_void, *mut c_void);
type CreateCodecFn = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type FormatNewFn = unsafe extern "C" fn() -> *mut c_void;
type SetInt32Fn = unsafe extern "C" fn(*mut c_void, *const c_char, i32);
type ConfigureFn =
    unsafe extern "C" fn(*mut c_void, *const c_void, *mut c_void, *mut c_void, u32) -> MediaStatus;
type DeleteFn = unsafe extern "C" fn(*mut c_void) -> MediaStatus;

struct Natives {
    new: NewFn,
    new_with_usage: NewWithUsageFn,
    get_window: ReaderOutFn,
    acquire_latest: ReaderOutFn,
    set_listener: ListenerFn,
    set_buffer_removed_listener: ReaderOutFn,
    delete: ReleaseFn,
    format: ImageIntFn,
    crop: CropFn,
    timestamp: TimestampFn,
    pixel_stride: PlaneIntFn,
    row_stride: PlaneIntFn,
    plane_data: PlaneDataFn,
    hardware_buffer: ReaderOutFn,
    image_delete: ReleaseFn,
    image_delete_async: DeleteAsyncFn,
    window_width: WindowQueryFn,
    window_height: WindowQueryFn,
    window_format: WindowQueryFn,
    describe: DescribeFn,
    buffer_acquire: ReleaseFn,
    buffer_release: ReleaseFn,
    create_codec: CreateCodecFn,
    format_new: FormatNewFn,
    format_delete: DeleteFn,
    set_int32: SetInt32Fn,
    configure: ConfigureFn,
    codec_delete: DeleteFn,
}

fn natives() -> &'static Natives {
    static NATIVES: OnceLock<Natives> = OnceLock::new();
    NATIVES.get_or_init(|| {
        let provider = EclipseNativeProvider::with_bionic_natives();
        Natives {
            new: native(&provider, "AImageReader_new"),
            new_with_usage: native(&provider, "AImageReader_newWithUsage"),
            get_window: native(&provider, "AImageReader_getWindow"),
            acquire_latest: native(&provider, "AImageReader_acquireLatestImage"),
            set_listener: native(&provider, "AImageReader_setImageListener"),
            set_buffer_removed_listener: native(&provider, "AImageReader_setBufferRemovedListener"),
            delete: native(&provider, "AImageReader_delete"),
            format: native(&provider, "AImage_getFormat"),
            crop: native(&provider, "AImage_getCropRect"),
            timestamp: native(&provider, "AImage_getTimestamp"),
            pixel_stride: native(&provider, "AImage_getPlanePixelStride"),
            row_stride: native(&provider, "AImage_getPlaneRowStride"),
            plane_data: native(&provider, "AImage_getPlaneData"),
            hardware_buffer: native(&provider, "AImage_getHardwareBuffer"),
            image_delete: native(&provider, "AImage_delete"),
            image_delete_async: native(&provider, "AImage_deleteAsync"),
            window_width: native(&provider, "ANativeWindow_getWidth"),
            window_height: native(&provider, "ANativeWindow_getHeight"),
            window_format: native(&provider, "ANativeWindow_getFormat"),
            describe: native(&provider, "AHardwareBuffer_describe"),
            buffer_acquire: native(&provider, "AHardwareBuffer_acquire"),
            buffer_release: native(&provider, "AHardwareBuffer_release"),
            create_codec: native(&provider, "AMediaCodec_createDecoderByType"),
            format_new: native(&provider, "AMediaFormat_new"),
            format_delete: native(&provider, "AMediaFormat_delete"),
            set_int32: native(&provider, "AMediaFormat_setInt32"),
            configure: native(&provider, "AMediaCodec_configure"),
            codec_delete: native(&provider, "AMediaCodec_delete"),
        }
    })
}

const WIDTH: i32 = 64;
const HEIGHT: i32 = 48;

struct TestReader(*mut c_void);

impl TestReader {
    fn new(max_images: i32) -> Self {
        let mut reader = std::ptr::null_mut();
        let status = unsafe {
            (natives().new)(
                WIDTH,
                HEIGHT,
                AIMAGE_FORMAT_YUV_420_888,
                max_images,
                &mut reader,
            )
        };
        assert_eq!(status, AMEDIA_OK);
        assert!(!reader.is_null());
        Self(reader)
    }

    fn window(&self) -> *mut c_void {
        let mut window = std::ptr::null_mut();
        assert_eq!(
            unsafe { (natives().get_window)(self.0, &mut window) },
            AMEDIA_OK
        );
        assert!(!window.is_null());
        window
    }

    fn acquire(&self) -> Result<TestImage, MediaStatus> {
        acquire_from(self.0)
    }

    fn listen(&self, context: *const c_void, callback: ImageCallback) {
        let mut listener = ImageListenerDesc {
            context: context.cast_mut(),
            on_image_available: Some(callback),
        };
        assert_eq!(
            unsafe { (natives().set_listener)(self.0, &mut listener) },
            AMEDIA_OK
        );
    }

    fn delete(&self) {
        unsafe { (natives().delete)(self.0) };
    }
}

impl Drop for TestReader {
    fn drop(&mut self) {
        self.delete();
    }
}

fn acquire_from(reader: *mut c_void) -> Result<TestImage, MediaStatus> {
    let mut image = std::ptr::null_mut();
    match unsafe { (natives().acquire_latest)(reader, &mut image) } {
        AMEDIA_OK => Ok(TestImage(image)),
        status => Err(status),
    }
}

struct TestImage(*mut c_void);

#[derive(Debug, PartialEq, Eq)]
struct Seen {
    format: i32,
    pixel_strides: [i32; 3],
    crop: [i32; 4],
    timestamp_ns: i64,
    nv12_sha256: String,
}

impl TestImage {
    fn int(&self, call: ImageIntFn) -> i32 {
        let mut value = 0;
        assert_eq!(unsafe { call(self.0, &mut value) }, AMEDIA_OK);
        value
    }

    fn plane_int(&self, call: PlaneIntFn, plane: c_int) -> i32 {
        let mut value = 0;
        assert_eq!(unsafe { call(self.0, plane, &mut value) }, AMEDIA_OK);
        value
    }

    fn plane(&self, plane: c_int) -> &[u8] {
        let mut data = std::ptr::null_mut();
        let mut length = 0;
        let status = unsafe { (natives().plane_data)(self.0, plane, &mut data, &mut length) };
        assert_eq!(status, AMEDIA_OK);
        assert!(!data.is_null());
        let length = usize::try_from(length).expect("a positive plane length");
        unsafe { std::slice::from_raw_parts(data, length) }
    }

    fn crop(&self) -> [i32; 4] {
        let mut rect = [-1; 4];
        assert_eq!(unsafe { (natives().crop)(self.0, &mut rect) }, AMEDIA_OK);
        rect
    }

    fn timestamp_ns(&self) -> i64 {
        let mut timestamp = -1;
        assert_eq!(
            unsafe { (natives().timestamp)(self.0, &mut timestamp) },
            AMEDIA_OK
        );
        timestamp
    }

    fn packed_nv12(&self) -> Vec<u8> {
        let [_, _, width, height] = self
            .crop()
            .map(|edge| usize::try_from(edge).expect("an edge"));
        let (chroma_width, chroma_height) = (width.div_ceil(2), height.div_ceil(2));
        let stride = |plane| usize::try_from(self.plane_int(natives().row_stride, plane)).unwrap();
        let step = |plane| usize::try_from(self.plane_int(natives().pixel_stride, plane)).unwrap();
        let mut packed = Vec::with_capacity(width * height + 2 * chroma_width * chroma_height);
        let luma = self.plane(0);
        for row in 0..height {
            packed.extend_from_slice(&luma[row * stride(0)..][..width]);
        }
        let (u, v) = (self.plane(1), self.plane(2));
        for row in 0..chroma_height {
            for column in 0..chroma_width {
                packed.push(u[row * stride(1) + column * step(1)]);
                packed.push(v[row * stride(2) + column * step(2)]);
            }
        }
        packed
    }

    fn seen(&self) -> Seen {
        Seen {
            format: self.int(natives().format),
            pixel_strides: [0, 1, 2].map(|plane| self.plane_int(natives().pixel_stride, plane)),
            crop: self.crop(),
            timestamp_ns: self.timestamp_ns(),
            nv12_sha256: sha256_hex(&self.packed_nv12()),
        }
    }
}

impl Drop for TestImage {
    fn drop(&mut self) {
        unsafe { (natives().image_delete)(self.0) };
    }
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[derive(Default)]
struct Deliveries {
    seen: Mutex<Vec<(ThreadId, Result<Seen, MediaStatus>)>>,
    changed: Condvar,
}

impl Deliveries {
    fn wait_for(&self, count: usize) {
        let seen = lock(&self.seen);
        let (seen, timeout) = self
            .changed
            .wait_timeout_while(seen, Duration::from_secs(10), |seen| seen.len() < count)
            .unwrap_or_else(PoisonError::into_inner);
        assert!(
            !timeout.timed_out(),
            "only {} of {count} images arrived",
            seen.len()
        );
    }
}

unsafe extern "C" fn record_latest_image(context: *mut c_void, reader: *mut c_void) {
    let deliveries = unsafe { &*context.cast::<Deliveries>() };
    let seen = acquire_from(reader).map(|image| image.seen());
    lock(&deliveries.seen).push((std::thread::current().id(), seen));
    deliveries.changed.notify_all();
}

#[test]
fn ndk_image_constants_match_the_headers() {
    assert_eq!(AIMAGE_FORMAT_YUV_420_888, 0x23);
    assert_eq!(AMEDIA_IMGREADER_NO_BUFFER_AVAILABLE, -30001);
    assert_eq!(AMEDIA_IMGREADER_MAX_IMAGES_ACQUIRED, -30002);
    assert_eq!(AHARDWAREBUFFER_DESC_SIZE, 40);
}

#[test]
fn decoded_h264_reaches_the_listener_as_nv12_images_on_another_thread() {
    if !decodable(
        "decoded_h264_reaches_the_listener_as_nv12_images_on_another_thread",
        Codec::H264,
    ) {
        return;
    }
    let deliveries = Deliveries::default();
    let reader = TestReader::new(2);
    reader.listen(std::ptr::from_ref(&deliveries).cast(), record_latest_image);
    let mut decoder = RenderingDecoder::start("h264-bframes", reader.window());
    let mut rendered = Vec::new();
    while let Some((pts_us, status)) = decoder.render_next() {
        assert_eq!(status, AMEDIA_OK);
        rendered.push(pts_us);
        deliveries.wait_for(rendered.len());
    }
    reader.delete();

    let fixture = decoder.fixture();
    assert_eq!(rendered.len(), fixture.frames);
    let seen = lock(&deliveries.seen);
    assert_eq!(seen.len(), fixture.frames, "one listener call per frame");
    let caller = std::thread::current().id();
    for (index, ((thread, image), pts_us)) in seen.iter().zip(&rendered).enumerate() {
        assert_ne!(*thread, caller, "frame {index} was announced on the caller");
        let expected = Seen {
            format: AIMAGE_FORMAT_YUV_420_888,
            pixel_strides: [1, 2, 2],
            crop: [0, 0, WIDTH, HEIGHT],
            timestamp_ns: pts_us * 1000,
            nv12_sha256: fixture.hashes().0[index].clone(),
        };
        assert_eq!(image.as_ref(), Ok(&expected), "frame {index}");
    }
}

#[test]
fn a_reader_hands_out_at_most_max_images_and_only_the_newest_frame() {
    if !decodable(
        "a_reader_hands_out_at_most_max_images_and_only_the_newest_frame",
        Codec::H264,
    ) {
        return;
    }
    let reader = TestReader::new(2);
    assert_eq!(
        reader.acquire().err(),
        Some(AMEDIA_IMGREADER_NO_BUFFER_AVAILABLE)
    );
    let mut decoder = RenderingDecoder::start("h264-bframes", reader.window());
    let mut render = || {
        let (pts_us, status) = decoder.render_next().expect("a frame");
        assert_eq!(status, AMEDIA_OK);
        pts_us
    };

    render();
    let first = reader.acquire().expect("the first image");
    render();
    let second = reader.acquire().expect("the second image");
    let third_pts = render();
    assert_eq!(
        reader.acquire().err(),
        Some(AMEDIA_IMGREADER_MAX_IMAGES_ACQUIRED)
    );
    drop(first);
    let third = reader.acquire().expect("a deleted image frees its place");
    assert_eq!(third.timestamp_ns(), third_pts * 1000);

    let mut buffer = std::ptr::dangling_mut();
    assert_eq!(
        unsafe { (natives().hardware_buffer)(third.0, &mut buffer) },
        AMEDIA_ERROR_UNSUPPORTED
    );
    assert!(buffer.is_null());
    let mut stride = 0;
    assert_eq!(
        unsafe { (natives().row_stride)(third.0, 3, &mut stride) },
        AMEDIA_ERROR_INVALID_PARAMETER
    );
    let mut length = 0;
    assert_eq!(
        unsafe { (natives().plane_data)(third.0, 0, std::ptr::null_mut(), &mut length) },
        AMEDIA_ERROR_INVALID_PARAMETER
    );
    drop((second, third));

    render();
    let newest_pts = render();
    let newest = reader.acquire().expect("the newest image");
    assert_eq!(newest.timestamp_ns(), newest_pts * 1000);
    assert_eq!(
        reader.acquire().err(),
        Some(AMEDIA_IMGREADER_NO_BUFFER_AVAILABLE),
        "acquiring the newest frame drops the older one"
    );
}

#[derive(Default)]
struct SlowListener {
    started: AtomicUsize,
    finished: AtomicUsize,
}

unsafe extern "C" fn sleep_in_listener(context: *mut c_void, _reader: *mut c_void) {
    let listener = unsafe { &*context.cast::<SlowListener>() };
    listener.started.fetch_add(1, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(200));
    listener.finished.fetch_add(1, Ordering::SeqCst);
}

#[test]
fn delete_waits_for_a_running_listener_and_silences_queued_ones() {
    if !decodable(
        "delete_waits_for_a_running_listener_and_silences_queued_ones",
        Codec::H264,
    ) {
        return;
    }
    let listener = SlowListener::default();
    let reader = TestReader::new(4);
    reader.listen(std::ptr::from_ref(&listener).cast(), sleep_in_listener);
    let mut decoder = RenderingDecoder::start("h264-bframes", reader.window());
    for _ in 0..3 {
        let (_, status) = decoder.render_next().expect("a frame");
        assert_eq!(status, AMEDIA_OK);
    }
    wait_until("the listener to start", || {
        listener.started.load(Ordering::SeqCst) == 1
    });

    reader.delete();
    assert_eq!(
        listener.finished.load(Ordering::SeqCst),
        1,
        "delete returned while the listener was still running"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        (
            listener.started.load(Ordering::SeqCst),
            listener.finished.load(Ordering::SeqCst)
        ),
        (1, 1),
        "no listener runs after delete returns"
    );
    let (_, status) = decoder.render_next().expect("a frame");
    assert_eq!(
        status, AMEDIA_ERROR_UNKNOWN,
        "a deleted reader takes no frames"
    );
}

unsafe extern "C" fn delete_own_reader(context: *mut c_void, reader: *mut c_void) {
    let calls = unsafe { &*context.cast::<AtomicUsize>() };
    unsafe { (natives().delete)(reader) };
    calls.fetch_add(1, Ordering::SeqCst);
}

#[test]
fn a_listener_may_delete_its_own_reader() {
    if !decodable("a_listener_may_delete_its_own_reader", Codec::H264) {
        return;
    }
    let calls = AtomicUsize::new(0);
    let reader = TestReader::new(2);
    reader.listen(std::ptr::from_ref(&calls).cast(), delete_own_reader);
    let window = reader.window();
    let mut decoder = RenderingDecoder::start("h264-bframes", window);
    let (_, status) = decoder.render_next().expect("a frame");
    assert_eq!(status, AMEDIA_OK);
    wait_until("the listener to delete its reader", || {
        calls.load(Ordering::SeqCst) == 1
    });
    assert_eq!(unsafe { (natives().window_width)(window) }, -1);
    let mut image = std::ptr::null_mut();
    assert_eq!(
        unsafe { (natives().acquire_latest)(reader.0, &mut image) },
        AMEDIA_ERROR_INVALID_OBJECT
    );
}

#[test]
fn the_reader_window_reports_the_reader_size_until_the_reader_is_deleted() {
    let reader = TestReader::new(1);
    let window = reader.window();
    assert_eq!(window, reader.window(), "the window is stable");
    let natives = natives();
    unsafe {
        assert_eq!((natives.window_width)(window), WIDTH);
        assert_eq!((natives.window_height)(window), HEIGHT);
        assert_eq!((natives.window_format)(window), AIMAGE_FORMAT_YUV_420_888);
    }
    reader.delete();
    unsafe {
        assert_eq!((natives.window_width)(window), -1);
        assert_eq!((natives.window_height)(window), -1);
        assert_eq!((natives.window_format)(window), -1);
    }
}

#[test]
fn readers_take_yuv_420_888_with_one_to_sixty_four_images() {
    let natives = natives();
    let mut reader = std::ptr::null_mut();
    let refused = [
        (WIDTH, HEIGHT, 1, 2),
        (WIDTH, HEIGHT, AIMAGE_FORMAT_YUV_420_888, 0),
        (WIDTH, HEIGHT, AIMAGE_FORMAT_YUV_420_888, 65),
        (0, HEIGHT, AIMAGE_FORMAT_YUV_420_888, 2),
        (WIDTH, -1, AIMAGE_FORMAT_YUV_420_888, 2),
    ];
    for (width, height, format, max_images) in refused {
        let status = unsafe { (natives.new)(width, height, format, max_images, &mut reader) };
        assert_eq!(
            status, AMEDIA_ERROR_INVALID_PARAMETER,
            "{width}x{height} format {format:#x} maxImages {max_images}"
        );
        assert!(reader.is_null());
    }
    assert_eq!(
        unsafe {
            (natives.new)(
                WIDTH,
                HEIGHT,
                AIMAGE_FORMAT_YUV_420_888,
                2,
                std::ptr::null_mut(),
            )
        },
        AMEDIA_ERROR_INVALID_PARAMETER
    );

    let usage = 3;
    let status = unsafe {
        (natives.new_with_usage)(
            WIDTH,
            HEIGHT,
            AIMAGE_FORMAT_YUV_420_888,
            usage,
            64,
            &mut reader,
        )
    };
    assert_eq!(status, AMEDIA_OK);
    let reader = TestReader(reader);
    assert_eq!(
        unsafe { (natives.get_window)(reader.0, std::ptr::null_mut()) },
        AMEDIA_ERROR_INVALID_PARAMETER
    );
    assert_eq!(
        unsafe { (natives.set_buffer_removed_listener)(reader.0, std::ptr::null_mut()) },
        AMEDIA_OK
    );
    assert_eq!(
        unsafe { (natives.set_listener)(reader.0, std::ptr::null_mut()) },
        AMEDIA_OK,
        "a NULL listener clears it"
    );
}

#[test]
fn stale_fabricated_and_null_handles_are_refused() {
    let natives = natives();
    let deleted = TestReader::new(1);
    deleted.delete();
    let mut out = std::ptr::null_mut();
    for (reader, status) in [
        (deleted.0, AMEDIA_ERROR_INVALID_OBJECT),
        (
            0xdead_beef_usize as *mut c_void,
            AMEDIA_ERROR_INVALID_OBJECT,
        ),
        (std::ptr::null_mut(), AMEDIA_ERROR_INVALID_PARAMETER),
    ] {
        unsafe {
            assert_eq!((natives.get_window)(reader, &mut out), status);
            assert_eq!((natives.acquire_latest)(reader, &mut out), status);
            assert_eq!((natives.set_listener)(reader, std::ptr::null_mut()), status);
            assert_eq!(
                (natives.set_buffer_removed_listener)(reader, std::ptr::null_mut()),
                status
            );
            (natives.delete)(reader);
        }
    }
    assert!(out.is_null(), "refused calls write nothing");

    for (image, status) in [
        (
            0xdead_beef_usize as *mut c_void,
            AMEDIA_ERROR_INVALID_OBJECT,
        ),
        (std::ptr::null_mut(), AMEDIA_ERROR_INVALID_PARAMETER),
    ] {
        let mut value = 0;
        let mut timestamp = 0;
        let mut rect = [0; 4];
        let mut data = std::ptr::null_mut();
        let mut length = 0;
        unsafe {
            assert_eq!((natives.format)(image, &mut value), status);
            assert_eq!((natives.crop)(image, &mut rect), status);
            assert_eq!((natives.timestamp)(image, &mut timestamp), status);
            assert_eq!((natives.pixel_stride)(image, 0, &mut value), status);
            assert_eq!((natives.row_stride)(image, 0, &mut value), status);
            assert_eq!(
                (natives.plane_data)(image, 0, &mut data, &mut length),
                status
            );
            assert_eq!((natives.hardware_buffer)(image, &mut out), status);
            (natives.image_delete)(image);
            (natives.image_delete_async)(image, -1);
        }
        assert_eq!((value, timestamp, rect, length), (0, 0, [0; 4], 0));
    }
}

#[test]
fn delete_async_closes_the_release_fence() {
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    let fence = unsafe { libc::fcntl(fds[0], libc::F_DUPFD_CLOEXEC, 900) };
    assert!(fence >= 900, "a high descriptor no other test reuses");
    for fd in fds {
        unsafe { libc::close(fd) };
    }
    unsafe { (natives().image_delete_async)(0xdead_beef_usize as *mut c_void, fence) };
    assert_eq!(unsafe { libc::fcntl(fence, libc::F_GETFD) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EBADF)
    );
}

#[test]
fn hardware_buffer_calls_only_zero_a_description() {
    let natives = natives();
    let mut desc = [0xaa_u8; AHARDWAREBUFFER_DESC_SIZE + 8];
    unsafe {
        (natives.describe)(std::ptr::null(), desc.as_mut_ptr().cast());
        (natives.describe)(std::ptr::null(), std::ptr::null_mut());
        (natives.buffer_acquire)(std::ptr::null_mut());
        (natives.buffer_release)(std::ptr::null_mut());
    }
    assert_eq!(
        desc[..AHARDWAREBUFFER_DESC_SIZE],
        [0; AHARDWAREBUFFER_DESC_SIZE]
    );
    assert_eq!(desc[AHARDWAREBUFFER_DESC_SIZE..], [0xaa; 8]);
}

#[test]
fn audio_decoders_cannot_render_into_a_reader() {
    if !decodable("audio_decoders_cannot_render_into_a_reader", Codec::Aac) {
        return;
    }
    let natives = natives();
    let reader = TestReader::new(1);
    let codec = unsafe { (natives.create_codec)(c"audio/mp4a-latm".as_ptr()) };
    assert!(!codec.is_null());
    let format = unsafe { (natives.format_new)() };
    let keys: [(&CStr, i32); 2] = [(c"sample-rate", 48_000), (c"channel-count", 2)];
    for (key, value) in keys {
        unsafe { (natives.set_int32)(format, key.as_ptr(), value) };
    }
    let status =
        unsafe { (natives.configure)(codec, format, reader.window(), std::ptr::null_mut(), 0) };
    assert_eq!(status, AMEDIA_ERROR_UNSUPPORTED);
    unsafe {
        assert_eq!((natives.format_delete)(format), AMEDIA_OK);
        assert_eq!((natives.codec_delete)(codec), AMEDIA_OK);
    }
}
