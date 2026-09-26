use std::cell::Cell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use super::ndk_registry::{NdkHandle, Slab};

const AAUDIO_OK: i32 = 0;
const AAUDIO_ERROR_DISCONNECTED: i32 = -899;
const AAUDIO_ERROR_ILLEGAL_ARGUMENT: i32 = -898;
const AAUDIO_ERROR_INVALID_STATE: i32 = -895;
const AAUDIO_ERROR_INVALID_HANDLE: i32 = -892;
const AAUDIO_ERROR_UNIMPLEMENTED: i32 = -890;
const AAUDIO_ERROR_UNAVAILABLE: i32 = -889;
const AAUDIO_ERROR_NO_MEMORY: i32 = -887;
const AAUDIO_ERROR_NULL: i32 = -886;
const AAUDIO_ERROR_TIMEOUT: i32 = -885;
const AAUDIO_ERROR_INVALID_FORMAT: i32 = -883;
const AAUDIO_ERROR_OUT_OF_RANGE: i32 = -882;

const AAUDIO_UNSPECIFIED: i32 = 0;

const AAUDIO_DIRECTION_OUTPUT: i32 = 0;
const AAUDIO_DIRECTION_INPUT: i32 = 1;

const AAUDIO_FORMAT_PCM_I16: i32 = 1;
const AAUDIO_FORMAT_PCM_FLOAT: i32 = 2;

const AAUDIO_PERFORMANCE_MODE_NONE: i32 = 10;
const AAUDIO_PERFORMANCE_MODE_POWER_SAVING: i32 = 11;
const AAUDIO_PERFORMANCE_MODE_LOW_LATENCY: i32 = 12;

const AAUDIO_USAGES: [i32; 17] = [
    AAUDIO_UNSPECIFIED,
    1,
    2,
    3,
    4,
    5,
    6,
    10,
    11,
    12,
    13,
    14,
    16,
    1000,
    1001,
    1002,
    1003,
];

const AAUDIO_INPUT_PRESETS: [i32; 9] = [AAUDIO_UNSPECIFIED, 1, 5, 6, 7, 9, 10, 1997, 1999];

const AAUDIO_CALLBACK_RESULT_CONTINUE: i32 = 0;

const STATE_POLL_INTERVAL: Duration = Duration::from_millis(5);

pub const AAUDIO_NATIVE_COUNT: usize = 26;

type DataCallback = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, i32) -> i32;

type ErrorCallback = unsafe extern "C" fn(*mut c_void, *mut c_void, i32);

#[derive(Clone, Copy)]
struct DataCallbackTarget {
    func: DataCallback,
    user_data: usize,
}

#[derive(Clone, Copy)]
struct ErrorCallbackTarget {
    func: ErrorCallback,
    user_data: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
enum StreamState {
    Open = 2,
    Started = 4,
    Paused = 6,
    Stopped = 10,
    Disconnected = 13,
}

impl StreamState {
    fn from_raw(raw: i32) -> Self {
        match raw {
            2 => Self::Open,
            4 => Self::Started,
            6 => Self::Paused,
            10 => Self::Stopped,
            13 => Self::Disconnected,
            _ => unreachable!("stream state words only ever hold StreamState values"),
        }
    }
}

struct AtomicStreamState(AtomicI32);

impl AtomicStreamState {
    fn new(state: StreamState) -> Self {
        Self(AtomicI32::new(state as i32))
    }

    fn load(&self) -> StreamState {
        StreamState::from_raw(self.0.load(Ordering::Acquire))
    }

    fn compare_exchange(&self, current: StreamState, new: StreamState) -> bool {
        self.0
            .compare_exchange(
                current as i32,
                new as i32,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn swap(&self, new: StreamState) -> StreamState {
        StreamState::from_raw(self.0.swap(new as i32, Ordering::AcqRel))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Request {
    Start,
    Pause,
    Stop,
}

impl Request {
    fn next_state(self, current: StreamState) -> Result<StreamState, i32> {
        match (self, current) {
            (Self::Start, StreamState::Open | StreamState::Paused | StreamState::Stopped) => {
                Ok(StreamState::Started)
            }
            (Self::Start, StreamState::Started | StreamState::Disconnected) => {
                Err(AAUDIO_ERROR_INVALID_STATE)
            }
            (Self::Pause | Self::Stop, StreamState::Disconnected) => Ok(StreamState::Disconnected),
            (Self::Pause, _) => Ok(StreamState::Paused),
            (Self::Stop, _) => Ok(StreamState::Stopped),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
    Output,
    Input,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SampleFormat {
    I16,
    Float,
}

impl SampleFormat {
    fn from_cpal(format: cpal::SampleFormat) -> Option<Self> {
        match format {
            cpal::SampleFormat::I16 => Some(Self::I16),
            cpal::SampleFormat::F32 => Some(Self::Float),
            _ => None,
        }
    }

    fn raw(self) -> i32 {
        match self {
            Self::I16 => AAUDIO_FORMAT_PCM_I16,
            Self::Float => AAUDIO_FORMAT_PCM_FLOAT,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeviceFormat {
    I16,
    I24,
    I32,
    Float,
}

impl DeviceFormat {
    const BY_PRECISION: [Self; 4] = [Self::Float, Self::I32, Self::I24, Self::I16];

    fn cpal(self) -> cpal::SampleFormat {
        match self {
            Self::I16 => cpal::SampleFormat::I16,
            Self::I24 => cpal::SampleFormat::I24,
            Self::I32 => cpal::SampleFormat::I32,
            Self::Float => cpal::SampleFormat::F32,
        }
    }
}

impl From<SampleFormat> for DeviceFormat {
    fn from(format: SampleFormat) -> Self {
        match format {
            SampleFormat::I16 => Self::I16,
            SampleFormat::Float => Self::Float,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PerformanceMode {
    None,
    PowerSaving,
    LowLatency,
}

impl PerformanceMode {
    fn burst_millis(self) -> u32 {
        match self {
            Self::LowLatency => 10,
            Self::None => 20,
            Self::PowerSaving => 40,
        }
    }
}

#[derive(Clone, Copy)]
struct BuilderSettings {
    direction: i32,
    format: i32,
    performance_mode: i32,
    usage: i32,
    input_preset: i32,
    buffer_capacity: i32,
    data_callback: Option<DataCallbackTarget>,
    error_callback: Option<ErrorCallbackTarget>,
}

impl Default for BuilderSettings {
    fn default() -> Self {
        Self {
            direction: AAUDIO_DIRECTION_OUTPUT,
            format: AAUDIO_UNSPECIFIED,
            performance_mode: AAUDIO_PERFORMANCE_MODE_NONE,
            usage: AAUDIO_UNSPECIFIED,
            input_preset: AAUDIO_UNSPECIFIED,
            buffer_capacity: AAUDIO_UNSPECIFIED,
            data_callback: None,
            error_callback: None,
        }
    }
}

#[derive(Clone, Copy)]
enum DataPath {
    Callback(DataCallbackTarget),
    ReadWrite,
}

#[derive(Clone, Copy)]
struct StreamParameters {
    direction: Direction,
    format: Option<SampleFormat>,
    performance_mode: PerformanceMode,
    data_path: DataPath,
    error_callback: Option<ErrorCallbackTarget>,
}

impl StreamParameters {
    fn parse(settings: &BuilderSettings) -> Result<Self, i32> {
        let direction = match settings.direction {
            AAUDIO_DIRECTION_OUTPUT => Direction::Output,
            AAUDIO_DIRECTION_INPUT => Direction::Input,
            _ => return Err(AAUDIO_ERROR_ILLEGAL_ARGUMENT),
        };
        let format = match settings.format {
            AAUDIO_UNSPECIFIED => None,
            AAUDIO_FORMAT_PCM_I16 => Some(SampleFormat::I16),
            AAUDIO_FORMAT_PCM_FLOAT => Some(SampleFormat::Float),
            _ => return Err(AAUDIO_ERROR_INVALID_FORMAT),
        };
        let performance_mode = match settings.performance_mode {
            AAUDIO_PERFORMANCE_MODE_NONE => PerformanceMode::None,
            AAUDIO_PERFORMANCE_MODE_POWER_SAVING => PerformanceMode::PowerSaving,
            AAUDIO_PERFORMANCE_MODE_LOW_LATENCY => PerformanceMode::LowLatency,
            _ => return Err(AAUDIO_ERROR_ILLEGAL_ARGUMENT),
        };
        if settings.buffer_capacity < 0 {
            return Err(AAUDIO_ERROR_OUT_OF_RANGE);
        }
        if !AAUDIO_USAGES.contains(&settings.usage)
            || !AAUDIO_INPUT_PRESETS.contains(&settings.input_preset)
        {
            return Err(AAUDIO_ERROR_ILLEGAL_ARGUMENT);
        }
        let data_path = match settings.data_callback {
            Some(callback) => DataPath::Callback(callback),
            None => DataPath::ReadWrite,
        };
        Ok(Self {
            direction,
            format,
            performance_mode,
            data_path,
            error_callback: settings.error_callback,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StreamFormat {
    channels: u16,
    sample_rate: u32,
    app: SampleFormat,
    device: DeviceFormat,
}

fn negotiate_format(
    requested: Option<SampleFormat>,
    device_default: &cpal::SupportedStreamConfig,
    ranges: impl IntoIterator<Item = cpal::SupportedStreamConfigRange>,
) -> Result<StreamFormat, i32> {
    let channels = device_default.channels();
    let sample_rate = device_default.sample_rate();
    let usable: Vec<cpal::SampleFormat> = ranges
        .into_iter()
        .filter(|range| range.channels() == channels)
        .filter_map(|range| range.try_with_sample_rate(sample_rate))
        .map(|config| config.sample_format())
        .collect();
    let app = requested
        .or_else(|| SampleFormat::from_cpal(device_default.sample_format()))
        .unwrap_or(SampleFormat::Float);
    let device = std::iter::once(DeviceFormat::from(app))
        .chain(DeviceFormat::BY_PRECISION)
        .find(|format| usable.contains(&format.cpal()))
        .ok_or(AAUDIO_ERROR_UNAVAILABLE)?;
    Ok(StreamFormat {
        channels,
        sample_rate,
        app,
        device,
    })
}

fn burst_frames(
    sample_rate: u32,
    mode: PerformanceMode,
    supported: &cpal::SupportedBufferSize,
) -> u32 {
    let exact = u64::from(sample_rate) * u64::from(mode.burst_millis()) / 1000;
    let frames = u32::try_from(exact.next_power_of_two()).unwrap_or(u32::MAX);
    match *supported {
        cpal::SupportedBufferSize::Range { min, max } => frames.clamp(min, max.max(min)),
        cpal::SupportedBufferSize::Unknown => frames,
    }
}

struct StreamShared {
    state: AtomicStreamState,
    frames_per_burst: AtomicI32,
    xrun_count: AtomicI32,
}

impl StreamShared {
    fn new(frames_per_burst: u32) -> Self {
        Self {
            state: AtomicStreamState::new(StreamState::Open),
            frames_per_burst: AtomicI32::new(frames_per_burst as i32),
            xrun_count: AtomicI32::new(0),
        }
    }

    fn capacity_frames(&self) -> i32 {
        2 * self.frames_per_burst.load(Ordering::Relaxed)
    }
}

struct StreamEntry {
    shared: Arc<StreamShared>,
    direction: Direction,
    data_path: DataPath,
    format: StreamFormat,
    host: Option<cpal::Stream>,
}

impl StreamEntry {
    fn apply(&self, request: Request) -> i32 {
        if request == Request::Pause && self.direction == Direction::Input {
            return AAUDIO_ERROR_UNIMPLEMENTED;
        }
        loop {
            let current = self.shared.state.load();
            let next = match request.next_state(current) {
                Ok(next) => next,
                Err(code) => return code,
            };
            if !self.shared.state.compare_exchange(current, next) {
                continue;
            }
            return match self.drive_host(next) {
                Ok(()) => AAUDIO_OK,
                Err(code) => {
                    self.shared.state.compare_exchange(next, current);
                    code
                }
            };
        }
    }

    fn drive_host(&self, state: StreamState) -> Result<(), i32> {
        let Some(host) = &self.host else {
            return Ok(());
        };
        if state == StreamState::Started {
            return host.play().map_err(|error| {
                tracing::warn!(target: "eclipse::audio", %error, "AAudio: host stream did not start");
                AAUDIO_ERROR_UNAVAILABLE
            });
        }
        if let Err(error) = host.pause() {
            tracing::debug!(
                target: "eclipse::audio",
                %error,
                "AAudio: host stream keeps running; the idle stream renders silence"
            );
        }
        Ok(())
    }
}

fn builders() -> &'static Slab<BuilderSettings> {
    static BUILDERS: Slab<BuilderSettings> = Slab::new();
    &BUILDERS
}

fn streams() -> &'static Slab<StreamEntry> {
    static STREAMS: Slab<StreamEntry> = Slab::new();
    &STREAMS
}

fn handle_of(ptr: *mut c_void) -> NdkHandle {
    ptr as usize as NdkHandle
}

fn ptr_of(handle: NdkHandle) -> *mut c_void {
    handle as usize as *mut c_void
}

thread_local! {
    static CALLBACK_STREAM: Cell<usize> = const { Cell::new(0) };
}

fn collides_with_callback(stream: *mut c_void) -> bool {
    !stream.is_null() && CALLBACK_STREAM.get() == stream as usize
}

struct Renderer<A> {
    shared: Arc<StreamShared>,
    callback: DataCallbackTarget,
    stream: usize,
    channels: usize,
    scratch: Box<[A]>,
}

impl<A: cpal::SizedSample> Renderer<A> {
    fn new(
        shared: Arc<StreamShared>,
        callback: DataCallbackTarget,
        stream: NdkHandle,
        format: StreamFormat,
    ) -> Self {
        let channels = usize::from(format.channels);
        let capacity = usize::try_from(shared.capacity_frames())
            .unwrap_or(0)
            .max(1);
        Self {
            shared,
            callback,
            stream: stream as usize,
            channels,
            scratch: vec![A::EQUILIBRIUM; capacity * channels].into_boxed_slice(),
        }
    }

    fn note_burst(&self, samples: usize) {
        let frames = (samples / self.channels) as i32;
        if frames > 0 && self.shared.frames_per_burst.load(Ordering::Relaxed) != frames {
            self.shared
                .frames_per_burst
                .store(frames, Ordering::Relaxed);
        }
    }

    fn invoke(&mut self, samples: usize) -> bool {
        if self.shared.state.load() != StreamState::Started {
            return false;
        }
        let frames = (samples / self.channels) as i32;
        let previous = CALLBACK_STREAM.replace(self.stream);
        let result = unsafe {
            (self.callback.func)(
                self.stream as *mut c_void,
                self.callback.user_data as *mut c_void,
                self.scratch.as_mut_ptr().cast(),
                frames,
            )
        };
        CALLBACK_STREAM.set(previous);
        if result != AAUDIO_CALLBACK_RESULT_CONTINUE {
            self.shared
                .state
                .compare_exchange(StreamState::Started, StreamState::Stopped);
        }
        true
    }

    fn render_output<T>(&mut self, out: &mut [T])
    where
        T: cpal::SizedSample + cpal::FromSample<A>,
    {
        self.note_burst(out.len());
        let chunk_len = self.scratch.len();
        for chunk in out.chunks_mut(chunk_len) {
            if self.invoke(chunk.len()) {
                for (sample, app) in chunk.iter_mut().zip(self.scratch.iter()) {
                    *sample = T::from_sample(*app);
                }
            } else {
                chunk.fill(T::EQUILIBRIUM);
            }
        }
    }

    fn capture_input<T>(&mut self, input: &[T])
    where
        T: cpal::SizedSample,
        A: cpal::FromSample<T>,
    {
        self.note_burst(input.len());
        let chunk_len = self.scratch.len();
        for chunk in input.chunks(chunk_len) {
            for (app, sample) in self.scratch.iter_mut().zip(chunk) {
                *app = A::from_sample(*sample);
            }
            self.invoke(chunk.len());
        }
    }
}

struct ErrorReporter {
    shared: Arc<StreamShared>,
    callback: Option<ErrorCallbackTarget>,
    stream: usize,
}

impl ErrorReporter {
    fn report(&self, error: cpal::Error) {
        match error.kind() {
            cpal::ErrorKind::Xrun => {
                self.shared.xrun_count.fetch_add(1, Ordering::Relaxed);
            }
            cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::StreamInvalidated => {
                if self.shared.state.swap(StreamState::Disconnected) == StreamState::Disconnected {
                    return;
                }
                tracing::warn!(target: "eclipse::audio", %error, "AAudio: stream disconnected");
                if let Some(target) = self.callback {
                    unsafe {
                        (target.func)(
                            self.stream as *mut c_void,
                            target.user_data as *mut c_void,
                            AAUDIO_ERROR_DISCONNECTED,
                        )
                    };
                }
            }
            _ => {
                tracing::warn!(target: "eclipse::audio", %error, "AAudio: host stream error");
            }
        }
    }
}

struct HostStreamPlan {
    format: StreamFormat,
    config: cpal::StreamConfig,
    direction: Direction,
}

fn build_host_stream(
    device: &cpal::Device,
    plan: &HostStreamPlan,
    params: &StreamParameters,
    data_callback: DataCallbackTarget,
    shared: &Arc<StreamShared>,
    stream: NdkHandle,
) -> Result<cpal::Stream, cpal::Error> {
    let reporter = ErrorReporter {
        shared: Arc::clone(shared),
        callback: params.error_callback,
        stream: stream as usize,
    };
    let shared = Arc::clone(shared);
    match plan.format.app {
        SampleFormat::I16 => build_for_app::<i16>(
            device,
            plan,
            Renderer::new(shared, data_callback, stream, plan.format),
            reporter,
        ),
        SampleFormat::Float => build_for_app::<f32>(
            device,
            plan,
            Renderer::new(shared, data_callback, stream, plan.format),
            reporter,
        ),
    }
}

fn build_for_app<A>(
    device: &cpal::Device,
    plan: &HostStreamPlan,
    renderer: Renderer<A>,
    reporter: ErrorReporter,
) -> Result<cpal::Stream, cpal::Error>
where
    A: cpal::SizedSample
        + cpal::FromSample<i16>
        + cpal::FromSample<cpal::I24>
        + cpal::FromSample<i32>
        + cpal::FromSample<f32>
        + Send
        + 'static,
    i16: cpal::FromSample<A>,
    cpal::I24: cpal::FromSample<A>,
    i32: cpal::FromSample<A>,
    f32: cpal::FromSample<A>,
{
    let config = plan.config;
    match (plan.direction, plan.format.device) {
        (Direction::Output, DeviceFormat::I16) => {
            build_output::<i16, A>(device, config, renderer, reporter)
        }
        (Direction::Output, DeviceFormat::I24) => {
            build_output::<cpal::I24, A>(device, config, renderer, reporter)
        }
        (Direction::Output, DeviceFormat::I32) => {
            build_output::<i32, A>(device, config, renderer, reporter)
        }
        (Direction::Output, DeviceFormat::Float) => {
            build_output::<f32, A>(device, config, renderer, reporter)
        }
        (Direction::Input, DeviceFormat::I16) => {
            build_input::<i16, A>(device, config, renderer, reporter)
        }
        (Direction::Input, DeviceFormat::I24) => {
            build_input::<cpal::I24, A>(device, config, renderer, reporter)
        }
        (Direction::Input, DeviceFormat::I32) => {
            build_input::<i32, A>(device, config, renderer, reporter)
        }
        (Direction::Input, DeviceFormat::Float) => {
            build_input::<f32, A>(device, config, renderer, reporter)
        }
    }
}

fn build_output<T, A>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut renderer: Renderer<A>,
    reporter: ErrorReporter,
) -> Result<cpal::Stream, cpal::Error>
where
    T: cpal::SizedSample + cpal::FromSample<A>,
    A: cpal::SizedSample + Send + 'static,
{
    device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| renderer.render_output(data),
        move |error| reporter.report(error),
        None,
    )
}

fn build_input<T, A>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut renderer: Renderer<A>,
    reporter: ErrorReporter,
) -> Result<cpal::Stream, cpal::Error>
where
    T: cpal::SizedSample,
    A: cpal::SizedSample + cpal::FromSample<T> + Send + 'static,
{
    device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| renderer.capture_input(data),
        move |error| reporter.report(error),
        None,
    )
}

fn plan_host_stream(
    device: &cpal::Device,
    params: &StreamParameters,
) -> Result<HostStreamPlan, i32> {
    let unavailable = |error: &dyn std::fmt::Display| {
        tracing::warn!(target: "eclipse::audio", %error, "AAudio: host device configuration unavailable");
        AAUDIO_ERROR_UNAVAILABLE
    };
    let (device_default, ranges): (_, Vec<_>) = match params.direction {
        Direction::Output => (
            device
                .default_output_config()
                .map_err(|e| unavailable(&e))?,
            device
                .supported_output_configs()
                .map_err(|e| unavailable(&e))?
                .collect(),
        ),
        Direction::Input => (
            device.default_input_config().map_err(|e| unavailable(&e))?,
            device
                .supported_input_configs()
                .map_err(|e| unavailable(&e))?
                .collect(),
        ),
    };
    let format = negotiate_format(params.format, &device_default, ranges)?;
    let burst = burst_frames(
        format.sample_rate,
        params.performance_mode,
        device_default.buffer_size(),
    );
    Ok(HostStreamPlan {
        format,
        config: cpal::StreamConfig {
            channels: format.channels,
            sample_rate: format.sample_rate,
            buffer_size: cpal::BufferSize::Fixed(burst),
        },
        direction: params.direction,
    })
}

fn open_stream(settings: &BuilderSettings) -> Result<NdkHandle, i32> {
    let params = StreamParameters::parse(settings)?;
    let host = cpal::default_host();
    let device = match params.direction {
        Direction::Output => host.default_output_device(),
        Direction::Input => host.default_input_device(),
    }
    .ok_or(AAUDIO_ERROR_UNAVAILABLE)?;
    let plan = plan_host_stream(&device, &params)?;
    let burst = match plan.config.buffer_size {
        cpal::BufferSize::Fixed(frames) => frames,
        cpal::BufferSize::Default => unreachable!("plan_host_stream always fixes the burst"),
    };
    let shared = Arc::new(StreamShared::new(burst));
    let handle = streams()
        .insert(StreamEntry {
            shared: Arc::clone(&shared),
            direction: params.direction,
            data_path: params.data_path,
            format: plan.format,
            host: None,
        })
        .map_err(|_| AAUDIO_ERROR_NO_MEMORY)?;
    if let DataPath::Callback(data_callback) = params.data_path {
        attach_host_stream(&device, &plan, &params, data_callback, &shared, handle)?;
    }
    tracing::info!(
        target: "eclipse::audio",
        direction = ?params.direction,
        channels = plan.format.channels,
        sample_rate = plan.format.sample_rate,
        format = ?plan.format.app,
        frames_per_burst = burst,
        "AAudio: stream opened"
    );
    Ok(handle)
}

fn attach_host_stream(
    device: &cpal::Device,
    plan: &HostStreamPlan,
    params: &StreamParameters,
    data_callback: DataCallbackTarget,
    shared: &Arc<StreamShared>,
    handle: NdkHandle,
) -> Result<(), i32> {
    let host_stream = match build_host_stream(device, plan, params, data_callback, shared, handle) {
        Ok(stream) => stream,
        Err(error) => {
            tracing::warn!(target: "eclipse::audio", %error, "AAudio: host stream could not be built");
            let _ = streams().remove(handle);
            return Err(AAUDIO_ERROR_UNAVAILABLE);
        }
    };
    streams()
        .with(handle, |entry| entry.host = Some(host_stream))
        .map_err(|_| AAUDIO_ERROR_INVALID_HANDLE)
}

fn with_builder(builder: *mut c_void, f: impl FnOnce(&mut BuilderSettings)) {
    if builder.is_null() || builders().with(handle_of(builder), f).is_err() {
        tracing::warn!(target: "eclipse::audio", "AAudio: builder setter called with an invalid builder");
    }
}

fn with_stream<R>(stream: *mut c_void, f: impl FnOnce(&mut StreamEntry) -> R) -> Result<R, i32> {
    if stream.is_null() {
        return Err(AAUDIO_ERROR_NULL);
    }
    streams()
        .with(handle_of(stream), f)
        .map_err(|_| AAUDIO_ERROR_INVALID_HANDLE)
}

fn stream_value(stream: *mut c_void, f: impl FnOnce(&mut StreamEntry) -> i32) -> i32 {
    with_stream(stream, f).unwrap_or_else(|code| code)
}

fn request(stream: *mut c_void, request: Request) -> i32 {
    if collides_with_callback(stream) {
        return AAUDIO_ERROR_INVALID_STATE;
    }
    stream_value(stream, |entry| entry.apply(request))
}

unsafe extern "C" fn create_stream_builder(builder: *mut *mut c_void) -> i32 {
    if builder.is_null() {
        return AAUDIO_ERROR_NULL;
    }
    match builders().insert(BuilderSettings::default()) {
        Ok(handle) => {
            unsafe { builder.write(ptr_of(handle)) };
            AAUDIO_OK
        }
        Err(_) => AAUDIO_ERROR_NO_MEMORY,
    }
}

unsafe extern "C" fn builder_delete(builder: *mut c_void) -> i32 {
    if builder.is_null() {
        return AAUDIO_ERROR_NULL;
    }
    match builders().remove(handle_of(builder)) {
        Ok(()) => AAUDIO_OK,
        Err(_) => AAUDIO_ERROR_INVALID_HANDLE,
    }
}

unsafe extern "C" fn builder_open_stream(builder: *mut c_void, stream: *mut *mut c_void) -> i32 {
    if builder.is_null() || stream.is_null() {
        return AAUDIO_ERROR_NULL;
    }
    unsafe { stream.write(std::ptr::null_mut()) };
    let Ok(settings) = builders().with(handle_of(builder), |settings| *settings) else {
        return AAUDIO_ERROR_INVALID_HANDLE;
    };
    match open_stream(&settings) {
        Ok(handle) => {
            unsafe { stream.write(ptr_of(handle)) };
            AAUDIO_OK
        }
        Err(code) => code,
    }
}

unsafe extern "C" fn builder_set_direction(builder: *mut c_void, direction: i32) {
    with_builder(builder, |settings| settings.direction = direction);
}

unsafe extern "C" fn builder_set_format(builder: *mut c_void, format: i32) {
    with_builder(builder, |settings| settings.format = format);
}

unsafe extern "C" fn builder_set_performance_mode(builder: *mut c_void, mode: i32) {
    with_builder(builder, |settings| settings.performance_mode = mode);
}

unsafe extern "C" fn builder_set_usage(builder: *mut c_void, usage: i32) {
    with_builder(builder, |settings| settings.usage = usage);
}

unsafe extern "C" fn builder_set_input_preset(builder: *mut c_void, preset: i32) {
    with_builder(builder, |settings| settings.input_preset = preset);
}

unsafe extern "C" fn builder_set_buffer_capacity_in_frames(builder: *mut c_void, frames: i32) {
    with_builder(builder, |settings| settings.buffer_capacity = frames);
}

unsafe extern "C" fn builder_set_data_callback(
    builder: *mut c_void,
    callback: Option<DataCallback>,
    user_data: *mut c_void,
) {
    with_builder(builder, |settings| {
        settings.data_callback = callback.map(|func| DataCallbackTarget {
            func,
            user_data: user_data as usize,
        });
    });
}

unsafe extern "C" fn builder_set_error_callback(
    builder: *mut c_void,
    callback: Option<ErrorCallback>,
    user_data: *mut c_void,
) {
    with_builder(builder, |settings| {
        settings.error_callback = callback.map(|func| ErrorCallbackTarget {
            func,
            user_data: user_data as usize,
        });
    });
}

unsafe extern "C" fn stream_close(stream: *mut c_void) -> i32 {
    if collides_with_callback(stream) {
        return AAUDIO_ERROR_INVALID_STATE;
    }
    let host = match with_stream(stream, |entry| entry.host.take()) {
        Ok(host) => host,
        Err(code) => return code,
    };
    drop(host);
    match streams().remove(handle_of(stream)) {
        Ok(()) => AAUDIO_OK,
        Err(_) => AAUDIO_ERROR_INVALID_HANDLE,
    }
}

unsafe extern "C" fn stream_request_start(stream: *mut c_void) -> i32 {
    request(stream, Request::Start)
}

unsafe extern "C" fn stream_request_pause(stream: *mut c_void) -> i32 {
    request(stream, Request::Pause)
}

unsafe extern "C" fn stream_request_stop(stream: *mut c_void) -> i32 {
    request(stream, Request::Stop)
}

unsafe extern "C" fn stream_get_state(stream: *mut c_void) -> i32 {
    stream_value(stream, |entry| entry.shared.state.load() as i32)
}

unsafe extern "C" fn stream_wait_for_state_change(
    stream: *mut c_void,
    input_state: i32,
    next_state: *mut i32,
    timeout_nanos: i64,
) -> i32 {
    let shared = match with_stream(stream, |entry| Arc::clone(&entry.shared)) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let timeout = Duration::from_nanos(u64::try_from(timeout_nanos).unwrap_or(0));
    let deadline = Instant::now() + timeout;
    let state = loop {
        let state = shared.state.load() as i32;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if state != input_state || remaining.is_zero() {
            break state;
        }
        std::thread::sleep(remaining.min(STATE_POLL_INTERVAL));
    };
    if !next_state.is_null() {
        unsafe { next_state.write(state) };
    }
    if state == input_state {
        AAUDIO_ERROR_TIMEOUT
    } else {
        AAUDIO_OK
    }
}

unsafe extern "C" fn stream_read(
    stream: *mut c_void,
    buffer: *mut c_void,
    frames: i32,
    _timeout_nanos: i64,
) -> i32 {
    if buffer.is_null() {
        return AAUDIO_ERROR_NULL;
    }
    if frames < 0 {
        return AAUDIO_ERROR_ILLEGAL_ARGUMENT;
    }
    if frames == 0 {
        return 0;
    }
    stream_value(stream, |entry| match (entry.direction, entry.data_path) {
        (Direction::Input, DataPath::Callback(_)) => AAUDIO_ERROR_INVALID_STATE,
        (Direction::Output, _) | (Direction::Input, DataPath::ReadWrite) => {
            AAUDIO_ERROR_UNIMPLEMENTED
        }
    })
}

unsafe extern "C" fn stream_get_format(stream: *mut c_void) -> i32 {
    stream_value(stream, |entry| entry.format.app.raw())
}

unsafe extern "C" fn stream_get_sample_rate(stream: *mut c_void) -> i32 {
    stream_value(stream, |entry| entry.format.sample_rate as i32)
}

unsafe extern "C" fn stream_get_channel_count(stream: *mut c_void) -> i32 {
    stream_value(stream, |entry| i32::from(entry.format.channels))
}

unsafe extern "C" fn stream_get_frames_per_burst(stream: *mut c_void) -> i32 {
    stream_value(stream, |entry| {
        entry.shared.frames_per_burst.load(Ordering::Relaxed)
    })
}

unsafe extern "C" fn stream_get_buffer_capacity_in_frames(stream: *mut c_void) -> i32 {
    stream_value(stream, |entry| entry.shared.capacity_frames())
}

unsafe extern "C" fn stream_get_buffer_size_in_frames(stream: *mut c_void) -> i32 {
    stream_value(stream, |entry| entry.shared.capacity_frames())
}

unsafe extern "C" fn stream_set_buffer_size_in_frames(stream: *mut c_void, _frames: i32) -> i32 {
    stream_value(stream, |entry| entry.shared.capacity_frames())
}

unsafe extern "C" fn stream_get_xrun_count(stream: *mut c_void) -> i32 {
    stream_value(stream, |entry| {
        entry.shared.xrun_count.load(Ordering::Relaxed)
    })
}

pub(crate) fn register_natives(mut register: impl FnMut(&'static str, u64)) {
    macro_rules! reg {
        ($name:literal, $f:expr) => {
            register($name, $f as *const () as u64);
        };
    }

    reg!("AAudio_createStreamBuilder", create_stream_builder);
    reg!("AAudioStreamBuilder_delete", builder_delete);
    reg!("AAudioStreamBuilder_openStream", builder_open_stream);
    reg!("AAudioStreamBuilder_setDirection", builder_set_direction);
    reg!("AAudioStreamBuilder_setFormat", builder_set_format);
    reg!(
        "AAudioStreamBuilder_setPerformanceMode",
        builder_set_performance_mode
    );
    reg!("AAudioStreamBuilder_setUsage", builder_set_usage);
    reg!(
        "AAudioStreamBuilder_setInputPreset",
        builder_set_input_preset
    );
    reg!(
        "AAudioStreamBuilder_setBufferCapacityInFrames",
        builder_set_buffer_capacity_in_frames
    );
    reg!(
        "AAudioStreamBuilder_setDataCallback",
        builder_set_data_callback
    );
    reg!(
        "AAudioStreamBuilder_setErrorCallback",
        builder_set_error_callback
    );

    reg!("AAudioStream_close", stream_close);
    reg!("AAudioStream_requestStart", stream_request_start);
    reg!("AAudioStream_requestPause", stream_request_pause);
    reg!("AAudioStream_requestStop", stream_request_stop);
    reg!("AAudioStream_getState", stream_get_state);
    reg!(
        "AAudioStream_waitForStateChange",
        stream_wait_for_state_change
    );
    reg!("AAudioStream_read", stream_read);
    reg!("AAudioStream_getFormat", stream_get_format);
    reg!("AAudioStream_getSampleRate", stream_get_sample_rate);
    reg!("AAudioStream_getChannelCount", stream_get_channel_count);
    reg!(
        "AAudioStream_getFramesPerBurst",
        stream_get_frames_per_burst
    );
    reg!(
        "AAudioStream_getBufferCapacityInFrames",
        stream_get_buffer_capacity_in_frames
    );
    reg!(
        "AAudioStream_getBufferSizeInFrames",
        stream_get_buffer_size_in_frames
    );
    reg!(
        "AAudioStream_setBufferSizeInFrames",
        stream_set_buffer_size_in_frames
    );
    reg!("AAudioStream_getXRunCount", stream_get_xrun_count);
}

#[cfg(test)]
mod tests;
