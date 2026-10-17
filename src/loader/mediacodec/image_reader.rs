use std::cell::Cell;
use std::collections::VecDeque;
use std::ffi::{c_int, c_void};
use std::fmt;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError, Weak};

use tracing::field::DisplayValue;

use super::avcodec::VideoFrame;
use super::{
    MediaStatus, AMEDIA_ERROR_INVALID_OBJECT, AMEDIA_ERROR_INVALID_PARAMETER, AMEDIA_ERROR_UNKNOWN,
    AMEDIA_ERROR_UNSUPPORTED, AMEDIA_OK,
};
use crate::loader::ndk_registry::{
    self, handle_to_ptr, ptr_to_handle, NativeWindowState, NdkHandle, NdkRegistryError,
    ReaderWindow, ReaderWindowsFull, Slab, MAX_READER_WINDOWS,
};

pub(super) const AIMAGE_FORMAT_YUV_420_888: i32 = 0x23;
const AMEDIA_IMGREADER_NO_BUFFER_AVAILABLE: MediaStatus = -30001;
const AMEDIA_IMGREADER_MAX_IMAGES_ACQUIRED: MediaStatus = -30002;

const MAX_IMAGES_PER_READER: usize = 64;
const MAX_LIVE_IMAGES: usize = MAX_READER_WINDOWS * MAX_IMAGES_PER_READER;
const LISTENER_QUEUE_CAPACITY: usize = 64;
const AHARDWAREBUFFER_DESC_SIZE: usize = 40;

pub(super) type ImageCallback = unsafe extern "C" fn(*mut c_void, *mut c_void);

#[repr(C)]
pub(super) struct ImageListenerDesc {
    pub(super) context: *mut c_void,
    pub(super) on_image_available: Option<ImageCallback>,
}

#[derive(Clone, Copy)]
struct Listener {
    context: *mut c_void,
    on_image_available: ImageCallback,
}

unsafe impl Send for Listener {}

#[derive(Debug)]
pub(super) enum ReaderError {
    Null(&'static str),
    NotAReader(NdkRegistryError),
    NotAnImage(NdkRegistryError),
    Format(i32),
    Size { width: i32, height: i32 },
    MaxImages(i32),
    TooManyReaders(ReaderWindowsFull),
    TooManyImages,
    NoBufferAvailable,
    MaxImagesAcquired(usize),
    Plane(c_int),
    TooLarge { what: &'static str, value: usize },
    NoHardwareBuffer,
    Registry(NdkRegistryError),
}

impl ReaderError {
    fn status(&self) -> MediaStatus {
        match self {
            Self::Null(_)
            | Self::Format(_)
            | Self::Size { .. }
            | Self::MaxImages(_)
            | Self::Plane(_) => AMEDIA_ERROR_INVALID_PARAMETER,
            Self::NotAReader(_) | Self::NotAnImage(_) => AMEDIA_ERROR_INVALID_OBJECT,
            Self::TooManyReaders(_) | Self::TooLarge { .. } | Self::Registry(_) => {
                AMEDIA_ERROR_UNKNOWN
            }
            Self::TooManyImages | Self::MaxImagesAcquired(_) => {
                AMEDIA_IMGREADER_MAX_IMAGES_ACQUIRED
            }
            Self::NoBufferAvailable => AMEDIA_IMGREADER_NO_BUFFER_AVAILABLE,
            Self::NoHardwareBuffer => AMEDIA_ERROR_UNSUPPORTED,
        }
    }
}

impl fmt::Display for ReaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null(what) => write!(f, "the {what} pointer is NULL"),
            Self::NotAReader(error) => write!(f, "not a live AImageReader: {error}"),
            Self::NotAnImage(error) => write!(f, "not a live AImage: {error}"),
            Self::Format(format) => write!(f, "image format {format:#x} is not YUV_420_888"),
            Self::Size { width, height } => {
                write!(f, "images need a positive size, not {width}x{height}")
            }
            Self::MaxImages(count) => write!(
                f,
                "maxImages {count} is outside 1 to {MAX_IMAGES_PER_READER}"
            ),
            Self::TooManyReaders(full) => full.fmt(f),
            Self::TooManyImages => write!(f, "{MAX_LIVE_IMAGES} AImages are already live"),
            Self::NoBufferAvailable => f.write_str("no frame is waiting"),
            Self::MaxImagesAcquired(count) => {
                write!(f, "the caller already holds the reader's {count} images")
            }
            Self::Plane(index) => write!(f, "YUV_420_888 images have no plane {index}"),
            Self::TooLarge { what, value } => write!(f, "a {what} of {value} does not fit an int"),
            Self::NoHardwareBuffer => f.write_str("Eclipse images have no AHardwareBuffer"),
            Self::Registry(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ReaderError {}

fn status_of<T>(result: &Result<T, ReaderError>) -> MediaStatus {
    result
        .as_ref()
        .map_or_else(ReaderError::status, |_| AMEDIA_OK)
}

fn failure<T>(result: &Result<T, ReaderError>) -> Option<DisplayValue<&ReaderError>> {
    result.as_ref().err().map(tracing::field::display)
}

unsafe fn write_output<T: Copy>(
    out: *mut T,
    what: &'static str,
    value: impl FnOnce() -> Result<T, ReaderError>,
) -> Result<T, ReaderError> {
    if out.is_null() {
        return Err(ReaderError::Null(what));
    }
    let value = value()?;
    unsafe { out.write(value) };
    Ok(value)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn int(what: &'static str, value: usize) -> Result<c_int, ReaderError> {
    c_int::try_from(value).map_err(|_| ReaderError::TooLarge { what, value })
}

struct Frames {
    queue: VecDeque<VideoFrame<'static>>,
    acquired: usize,
}

enum Gate {
    Open(Option<Listener>),
    Closed,
}

struct Callbacks {
    gate: Gate,
    running: bool,
}

struct ReaderShared {
    max_images: usize,
    frames: Mutex<Frames>,
    callbacks: Mutex<Callbacks>,
    callback_finished: Condvar,
}

impl ReaderShared {
    fn new(max_images: usize) -> Self {
        Self {
            max_images,
            frames: Mutex::new(Frames {
                queue: VecDeque::with_capacity(max_images),
                acquired: 0,
            }),
            callbacks: Mutex::new(Callbacks {
                gate: Gate::Open(None),
                running: false,
            }),
            callback_finished: Condvar::new(),
        }
    }

    fn push(&self, frame: VideoFrame<'static>) {
        let mut frames = lock(&self.frames);
        if frames.queue.len() >= self.max_images {
            frames.queue.pop_front();
        }
        frames.queue.push_back(frame);
    }

    fn take_latest(&self) -> Result<VideoFrame<'static>, ReaderError> {
        let mut frames = lock(&self.frames);
        if frames.acquired >= self.max_images {
            return Err(ReaderError::MaxImagesAcquired(self.max_images));
        }
        let latest = frames
            .queue
            .pop_back()
            .ok_or(ReaderError::NoBufferAvailable)?;
        frames.queue.clear();
        frames.acquired += 1;
        Ok(latest)
    }

    fn return_image(&self) {
        let mut frames = lock(&self.frames);
        frames.acquired = frames.acquired.saturating_sub(1);
    }

    fn listening(&self) -> bool {
        matches!(lock(&self.callbacks).gate, Gate::Open(Some(_)))
    }

    fn set_listener(&self, listener: Option<Listener>) {
        if let Gate::Open(current) = &mut lock(&self.callbacks).gate {
            *current = listener;
        }
    }

    fn notify(&self, reader: NdkHandle) {
        let listener = {
            let mut callbacks = lock(&self.callbacks);
            let Gate::Open(Some(listener)) = callbacks.gate else {
                return;
            };
            callbacks.running = true;
            listener
        };
        unsafe { (listener.on_image_available)(listener.context, handle_to_ptr(reader)) };
        lock(&self.callbacks).running = false;
        self.callback_finished.notify_all();
    }

    fn close(&self) {
        lock(&self.frames).queue.clear();
        let mut callbacks = lock(&self.callbacks);
        callbacks.gate = Gate::Closed;
        if on_listener_thread() {
            return;
        }
        while callbacks.running {
            callbacks = self
                .callback_finished
                .wait(callbacks)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

fn window_of(shared: &Arc<ReaderShared>) -> usize {
    Arc::as_ptr(shared) as usize
}

fn readers() -> &'static Slab<Arc<ReaderShared>> {
    static READERS: Slab<Arc<ReaderShared>> = Slab::new();
    &READERS
}

fn live_reader(reader: *mut c_void) -> Result<Arc<ReaderShared>, ReaderError> {
    if reader.is_null() {
        return Err(ReaderError::Null("reader"));
    }
    readers()
        .with(ptr_to_handle(reader), |shared| Arc::clone(shared))
        .map_err(ReaderError::NotAReader)
}

fn create(width: i32, height: i32, format: i32, max_images: i32) -> Result<NdkHandle, ReaderError> {
    if format != AIMAGE_FORMAT_YUV_420_888 {
        return Err(ReaderError::Format(format));
    }
    if width < 1 || height < 1 {
        return Err(ReaderError::Size { width, height });
    }
    let max_images = usize::try_from(max_images)
        .ok()
        .filter(|count| (1..=MAX_IMAGES_PER_READER).contains(count))
        .ok_or(ReaderError::MaxImages(max_images))?;
    let shared = Arc::new(ReaderShared::new(max_images));
    let handle = readers()
        .insert(Arc::clone(&shared))
        .map_err(ReaderError::Registry)?;
    let window = ReaderWindow {
        reader: handle,
        state: NativeWindowState {
            width,
            height,
            format,
        },
    };
    if let Err(full) = ndk_registry::register_reader_window(window_of(&shared), window) {
        readers().remove(handle).map_err(ReaderError::Registry)?;
        return Err(ReaderError::TooManyReaders(full));
    }
    Ok(handle)
}

fn delete_reader(handle: *mut c_void) -> Result<(), ReaderError> {
    let shared = live_reader(handle)?;
    readers()
        .remove(ptr_to_handle(handle))
        .map_err(ReaderError::NotAReader)?;
    ndk_registry::unregister_reader_window(window_of(&shared));
    shared.close();
    Ok(())
}

pub(super) fn render(reader: NdkHandle, frame: VideoFrame<'static>) -> Result<(), ReaderError> {
    let shared = readers()
        .with(reader, |shared| Arc::clone(shared))
        .map_err(ReaderError::NotAReader)?;
    shared.push(frame);
    if shared.listening() {
        announce(reader, shared);
    }
    Ok(())
}

thread_local! {
    static ON_LISTENER_THREAD: Cell<bool> = const { Cell::new(false) };
}

fn on_listener_thread() -> bool {
    ON_LISTENER_THREAD.try_with(Cell::get).unwrap_or(false)
}

struct Notification {
    reader: NdkHandle,
    shared: Arc<ReaderShared>,
}

fn listener_queue() -> Option<&'static SyncSender<Notification>> {
    static QUEUE: OnceLock<Option<SyncSender<Notification>>> = OnceLock::new();
    QUEUE.get_or_init(start_listener_thread).as_ref()
}

fn start_listener_thread() -> Option<SyncSender<Notification>> {
    let (sender, notifications) = mpsc::sync_channel(LISTENER_QUEUE_CAPACITY);
    let started = std::thread::Builder::new()
        .name("media-image-listener".to_owned())
        .spawn(move || deliver(&notifications));
    match started {
        Ok(_) => Some(sender),
        Err(error) => {
            tracing::error!(
                target: "mediandk",
                %error,
                "the AImageReader listener thread did not start, so image listeners never run"
            );
            None
        }
    }
}

fn deliver(notifications: &Receiver<Notification>) {
    ON_LISTENER_THREAD.set(true);
    for Notification { reader, shared } in notifications {
        shared.notify(reader);
    }
}

fn announce(reader: NdkHandle, shared: Arc<ReaderShared>) {
    let Some(queue) = listener_queue() else {
        return;
    };
    match queue.try_send(Notification { reader, shared }) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => tracing::debug!(
            target: "mediandk",
            reader = ?handle_to_ptr::<c_void>(reader),
            "the image listener queue is full; the frame stays acquirable"
        ),
        Err(TrySendError::Disconnected(_)) => tracing::warn!(
            target: "mediandk",
            reader = ?handle_to_ptr::<c_void>(reader),
            "the AImageReader listener thread is gone; the frame stays acquirable"
        ),
    }
}

struct Image {
    frame: VideoFrame<'static>,
    chroma: Vec<u8>,
    reader: Weak<ReaderShared>,
}

struct PlaneView {
    data: *const u8,
    len: usize,
    row_stride: usize,
    pixel_stride: usize,
}

impl Image {
    fn new(frame: VideoFrame<'static>, reader: Weak<ReaderShared>) -> Self {
        let (chroma_width, chroma_height) = frame.format().chroma_size();
        let mut chroma = Vec::with_capacity(2 * chroma_width * chroma_height);
        frame.extend_with_interleaved_chroma(&mut chroma);
        Self {
            frame,
            chroma,
            reader,
        }
    }

    fn plane(&self, index: c_int) -> Result<PlaneView, ReaderError> {
        let chroma = |offset: usize| {
            let (chroma_width, _) = self.frame.format().chroma_size();
            PlaneView {
                data: self.chroma[offset..].as_ptr(),
                len: self.chroma.len() - 1,
                row_stride: 2 * chroma_width,
                pixel_stride: 2,
            }
        };
        match index {
            0 => {
                let luma = self.frame.luma();
                Ok(PlaneView {
                    data: luma.bytes().as_ptr(),
                    len: luma.bytes().len(),
                    row_stride: luma.stride(),
                    pixel_stride: 1,
                })
            }
            1 => Ok(chroma(0)),
            2 => Ok(chroma(1)),
            other => Err(ReaderError::Plane(other)),
        }
    }

    fn crop_rect(&self) -> Result<[i32; 4], ReaderError> {
        let format = self.frame.format();
        Ok([
            0,
            0,
            int("width", format.width)?,
            int("height", format.height)?,
        ])
    }

    fn timestamp_ns(&self) -> i64 {
        self.frame.pts_us().unwrap_or(0).saturating_mul(1000)
    }
}

struct Images {
    slab: Slab<Image>,
    live: AtomicUsize,
}

impl Images {
    const fn new() -> Self {
        Self {
            slab: Slab::new(),
            live: AtomicUsize::new(0),
        }
    }

    fn insert(&self, image: Image) -> Result<NdkHandle, ReaderError> {
        self.live
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |live| {
                (live < MAX_LIVE_IMAGES).then_some(live + 1)
            })
            .map_err(|_| ReaderError::TooManyImages)?;
        self.slab.insert(image).map_err(|error| {
            self.live.fetch_sub(1, Ordering::SeqCst);
            ReaderError::Registry(error)
        })
    }

    fn with<R>(
        &self,
        image: *const c_void,
        f: impl FnOnce(&Image) -> Result<R, ReaderError>,
    ) -> Result<R, ReaderError> {
        if image.is_null() {
            return Err(ReaderError::Null("image"));
        }
        self.slab
            .with(ptr_to_handle(image), |image| f(image))
            .map_err(ReaderError::NotAnImage)?
    }

    fn delete(&self, image: *mut c_void) -> Result<(), ReaderError> {
        let reader = self.with(image, |image| Ok(image.reader.clone()))?;
        self.slab
            .remove(ptr_to_handle(image))
            .map_err(ReaderError::NotAnImage)?;
        self.live.fetch_sub(1, Ordering::SeqCst);
        if let Some(reader) = reader.upgrade() {
            reader.return_image();
        }
        Ok(())
    }
}

fn images() -> &'static Images {
    static IMAGES: Images = Images::new();
    &IMAGES
}

fn acquire_latest(handle: *mut c_void) -> Result<NdkHandle, ReaderError> {
    let shared = live_reader(handle)?;
    let frame = shared.take_latest()?;
    let image = Image::new(frame, Arc::downgrade(&shared));
    images()
        .insert(image)
        .inspect_err(|_| shared.return_image())
}

unsafe fn new_reader(
    call: &'static str,
    width: i32,
    height: i32,
    format: i32,
    usage: Option<u64>,
    max_images: i32,
    out: *mut *mut c_void,
) -> MediaStatus {
    let result = unsafe {
        write_output(out, "reader output", || {
            create(width, height, format, max_images).map(handle_to_ptr)
        })
    };
    let status = status_of(&result);
    tracing::debug!(
        target: "mediandk",
        width,
        height,
        format,
        ?usage,
        max_images,
        reader = ?result.as_ref().ok(),
        status,
        error = failure(&result),
        "{call}"
    );
    status
}

pub(super) unsafe extern "C" fn eclipse_aimagereader_new(
    width: i32,
    height: i32,
    format: i32,
    max_images: i32,
    reader: *mut *mut c_void,
) -> MediaStatus {
    unsafe {
        new_reader(
            "AImageReader_new",
            width,
            height,
            format,
            None,
            max_images,
            reader,
        )
    }
}

pub(super) unsafe extern "C" fn eclipse_aimagereader_newwithusage(
    width: i32,
    height: i32,
    format: i32,
    usage: u64,
    max_images: i32,
    reader: *mut *mut c_void,
) -> MediaStatus {
    unsafe {
        new_reader(
            "AImageReader_newWithUsage",
            width,
            height,
            format,
            Some(usage),
            max_images,
            reader,
        )
    }
}

pub(super) unsafe extern "C" fn eclipse_aimagereader_getwindow(
    handle: *mut c_void,
    window: *mut *mut c_void,
) -> MediaStatus {
    let result = unsafe {
        write_output(window, "window output", || {
            live_reader(handle).map(|shared| window_of(&shared) as *mut c_void)
        })
    };
    let status = status_of(&result);
    tracing::debug!(
        target: "mediandk",
        reader = ?handle,
        window = ?result.as_ref().ok(),
        status,
        error = failure(&result),
        "AImageReader_getWindow"
    );
    status
}

pub(super) unsafe extern "C" fn eclipse_aimagereader_acquirelatestimage(
    handle: *mut c_void,
    image: *mut *mut c_void,
) -> MediaStatus {
    let result = unsafe {
        write_output(image, "image output", || {
            acquire_latest(handle).map(handle_to_ptr)
        })
    };
    let status = status_of(&result);
    tracing::trace!(
        target: "mediandk",
        reader = ?handle,
        image = ?result.as_ref().ok(),
        status,
        error = failure(&result),
        "AImageReader_acquireLatestImage"
    );
    status
}

pub(super) unsafe extern "C" fn eclipse_aimagereader_setimagelistener(
    handle: *mut c_void,
    listener: *mut c_void,
) -> MediaStatus {
    let listener = (!listener.is_null())
        .then(|| unsafe { listener.cast::<ImageListenerDesc>().read() })
        .and_then(|desc| {
            desc.on_image_available.map(|on_image_available| Listener {
                context: desc.context,
                on_image_available,
            })
        });
    let result = live_reader(handle).map(|shared| shared.set_listener(listener));
    let status = status_of(&result);
    tracing::debug!(
        target: "mediandk",
        reader = ?handle,
        listening = listener.is_some(),
        status,
        error = failure(&result),
        "AImageReader_setImageListener"
    );
    status
}

pub(super) unsafe extern "C" fn eclipse_aimagereader_setbufferremovedlistener(
    handle: *mut c_void,
    listener: *mut c_void,
) -> MediaStatus {
    let result = live_reader(handle).map(drop);
    let status = status_of(&result);
    tracing::debug!(
        target: "mediandk",
        reader = ?handle,
        ?listener,
        status,
        error = failure(&result),
        "AImageReader_setBufferRemovedListener"
    );
    status
}

pub(super) unsafe extern "C" fn eclipse_aimagereader_delete(handle: *mut c_void) {
    let result = delete_reader(handle);
    tracing::debug!(
        target: "mediandk",
        reader = ?handle,
        error = failure(&result),
        "AImageReader_delete"
    );
}

unsafe fn image_value<T: Copy + fmt::Debug>(
    call: &'static str,
    image: *mut c_void,
    out: *mut T,
    value: impl FnOnce(&Image) -> Result<T, ReaderError>,
) -> MediaStatus {
    let result = unsafe { write_output(out, "output", || images().with(image, value)) };
    let status = status_of(&result);
    tracing::trace!(
        target: "mediandk",
        ?image,
        value = ?result.as_ref().ok(),
        status,
        error = failure(&result),
        "{call}"
    );
    status
}

pub(super) unsafe extern "C" fn eclipse_aimage_getformat(
    image: *mut c_void,
    format: *mut i32,
) -> MediaStatus {
    unsafe {
        image_value("AImage_getFormat", image, format, |_| {
            Ok(AIMAGE_FORMAT_YUV_420_888)
        })
    }
}

pub(super) unsafe extern "C" fn eclipse_aimage_getcroprect(
    image: *mut c_void,
    rect: *mut [i32; 4],
) -> MediaStatus {
    unsafe { image_value("AImage_getCropRect", image, rect, Image::crop_rect) }
}

pub(super) unsafe extern "C" fn eclipse_aimage_gettimestamp(
    image: *mut c_void,
    timestamp_ns: *mut i64,
) -> MediaStatus {
    unsafe {
        image_value("AImage_getTimestamp", image, timestamp_ns, |image| {
            Ok(image.timestamp_ns())
        })
    }
}

pub(super) unsafe extern "C" fn eclipse_aimage_getplanepixelstride(
    image: *mut c_void,
    plane: c_int,
    pixel_stride: *mut i32,
) -> MediaStatus {
    unsafe {
        image_value("AImage_getPlanePixelStride", image, pixel_stride, |image| {
            int("pixel stride", image.plane(plane)?.pixel_stride)
        })
    }
}

pub(super) unsafe extern "C" fn eclipse_aimage_getplanerowstride(
    image: *mut c_void,
    plane: c_int,
    row_stride: *mut i32,
) -> MediaStatus {
    unsafe {
        image_value("AImage_getPlaneRowStride", image, row_stride, |image| {
            int("row stride", image.plane(plane)?.row_stride)
        })
    }
}

unsafe fn plane_data(
    image: *mut c_void,
    plane: c_int,
    data: *mut *mut u8,
    data_length: *mut c_int,
) -> Result<c_int, ReaderError> {
    if data.is_null() || data_length.is_null() {
        return Err(ReaderError::Null("plane output"));
    }
    let (found, length) = images().with(image, |image| {
        let view = image.plane(plane)?;
        Ok((view.data, int("plane length", view.len)?))
    })?;
    unsafe {
        data.write(found.cast_mut());
        data_length.write(length);
    }
    Ok(length)
}

pub(super) unsafe extern "C" fn eclipse_aimage_getplanedata(
    image: *mut c_void,
    plane: c_int,
    data: *mut *mut u8,
    data_length: *mut c_int,
) -> MediaStatus {
    let result = unsafe { plane_data(image, plane, data, data_length) };
    let status = status_of(&result);
    tracing::trace!(
        target: "mediandk",
        ?image,
        plane,
        length = result.as_ref().ok(),
        status,
        error = failure(&result),
        "AImage_getPlaneData"
    );
    status
}

pub(super) unsafe extern "C" fn eclipse_aimage_gethardwarebuffer(
    image: *mut c_void,
    buffer: *mut *mut c_void,
) -> MediaStatus {
    let result: Result<(), ReaderError> = if buffer.is_null() {
        Err(ReaderError::Null("buffer output"))
    } else {
        unsafe { buffer.write(std::ptr::null_mut()) };
        images().with(image, |_| Err(ReaderError::NoHardwareBuffer))
    };
    let status = status_of(&result);
    tracing::debug!(
        target: "mediandk",
        ?image,
        status,
        error = failure(&result),
        "AImage_getHardwareBuffer"
    );
    status
}

pub(super) unsafe extern "C" fn eclipse_aimage_delete(image: *mut c_void) {
    let result = images().delete(image);
    tracing::trace!(
        target: "mediandk",
        ?image,
        error = failure(&result),
        "AImage_delete"
    );
}

pub(super) unsafe extern "C" fn eclipse_aimage_deleteasync(
    image: *mut c_void,
    release_fence: c_int,
) {
    if release_fence >= 0 {
        drop(unsafe { OwnedFd::from_raw_fd(release_fence) });
    }
    let result = images().delete(image);
    tracing::trace!(
        target: "mediandk",
        ?image,
        release_fence,
        error = failure(&result),
        "AImage_deleteAsync"
    );
}

pub(super) unsafe extern "C" fn eclipse_ahardwarebuffer_acquire(buffer: *mut c_void) {
    tracing::debug!(
        target: "mediandk",
        ?buffer,
        "AHardwareBuffer_acquire on a buffer Eclipse never made"
    );
}

pub(super) unsafe extern "C" fn eclipse_ahardwarebuffer_release(buffer: *mut c_void) {
    tracing::debug!(
        target: "mediandk",
        ?buffer,
        "AHardwareBuffer_release on a buffer Eclipse never made"
    );
}

pub(super) unsafe extern "C" fn eclipse_ahardwarebuffer_describe(
    buffer: *const c_void,
    desc: *mut c_void,
) {
    if !desc.is_null() {
        unsafe { desc.cast::<u8>().write_bytes(0, AHARDWAREBUFFER_DESC_SIZE) };
    }
    tracing::debug!(
        target: "mediandk",
        ?buffer,
        "AHardwareBuffer_describe on a buffer Eclipse never made"
    );
}

#[cfg(test)]
mod tests;
