use std::cell::Cell;
use std::ffi::c_void;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, StreamTrait};
use eclipse_config::audio::Direction;

use super::ndk_registry::{NdkHandle, Slab};
use crate::audio::{InputSource, RecordingSource};
use crate::diagnostics::MICROPHONE_TARGET;

pub mod harness;

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

const START_GRACE: Duration = Duration::from_secs(1);

const LATE_CALLBACK_LOG_INTERVAL: Duration = Duration::from_secs(10);

const LEVEL_REPORT_DELAY: Duration = Duration::from_secs(5);

const LEVEL_REPORT_THREAD: &str = "eclipse-mic-level";

pub(crate) const APP_SAMPLE_RATE: u32 = 48_000;

pub(crate) const APP_CHANNELS: u16 = 2;

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

impl ErrorCallbackTarget {
    fn report_disconnected(self, stream: usize) {
        unsafe {
            (self.func)(
                stream as *mut c_void,
                self.user_data as *mut c_void,
                AAUDIO_ERROR_DISCONNECTED,
            )
        };
    }
}

static OUTPUT_ROUTE_CHANGES: AtomicU64 = AtomicU64::new(0);

static INPUT_ROUTE_CHANGES: AtomicU64 = AtomicU64::new(0);

fn route_changes(direction: Direction) -> &'static AtomicU64 {
    match direction {
        Direction::Output => &OUTPUT_ROUTE_CHANGES,
        Direction::Input => &INPUT_ROUTE_CHANGES,
    }
}

pub fn reopen_streams(direction: Direction) {
    route_changes(direction).fetch_add(1, Ordering::AcqRel);
}

#[derive(Clone, Copy)]
struct Route {
    changes: &'static AtomicU64,
    opened_at: u64,
}

impl Route {
    fn of(direction: Direction) -> Self {
        Self::on(route_changes(direction))
    }

    fn on(changes: &'static AtomicU64) -> Self {
        Self {
            changes,
            opened_at: changes.load(Ordering::Acquire),
        }
    }

    fn changed(self) -> bool {
        self.changes.load(Ordering::Acquire) != self.opened_at
    }
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

impl DataPath {
    fn label(self) -> &'static str {
        match self {
            Self::Callback(_) => "callback",
            Self::ReadWrite => "read_write",
        }
    }
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
    let ranges: Vec<cpal::SupportedStreamConfigRange> = ranges.into_iter().collect();
    let app = requested
        .or_else(|| SampleFormat::from_cpal(device_default.sample_format()))
        .unwrap_or(SampleFormat::Float);
    let layouts = [
        (APP_CHANNELS, APP_SAMPLE_RATE),
        (device_default.channels(), device_default.sample_rate()),
    ];
    layouts
        .into_iter()
        .find_map(|(channels, sample_rate)| {
            Some(StreamFormat {
                channels,
                sample_rate,
                app,
                device: device_format(app, &ranges, channels, sample_rate)?,
            })
        })
        .ok_or(AAUDIO_ERROR_UNAVAILABLE)
}

fn device_format(
    app: SampleFormat,
    ranges: &[cpal::SupportedStreamConfigRange],
    channels: u16,
    sample_rate: u32,
) -> Option<DeviceFormat> {
    std::iter::once(DeviceFormat::from(app))
        .chain(DeviceFormat::BY_PRECISION)
        .find(|format| {
            ranges.iter().any(|range| {
                range.channels() == channels
                    && range.contains_rate(sample_rate)
                    && range.sample_format() == format.cpal()
            })
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

pub(crate) fn low_latency_burst_frames() -> u32 {
    burst_frames(
        APP_SAMPLE_RATE,
        PerformanceMode::LowLatency,
        &cpal::SupportedBufferSize::Unknown,
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Run(u64);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct BlockLevel {
    samples: u64,
    zero_samples: u64,
    peak: f32,
}

impl BlockLevel {
    fn add(&mut self, sample: f32) {
        let level = sample.abs();
        self.samples += 1;
        self.zero_samples += u64::from(level == 0.0);
        self.peak = self.peak.max(level);
    }
}

#[derive(Default)]
struct InputLevels {
    samples: AtomicU64,
    zero_samples: AtomicU64,
    peak_bits: AtomicU32,
}

impl InputLevels {
    fn add(&self, block: BlockLevel) {
        self.samples.fetch_add(block.samples, Ordering::Relaxed);
        self.zero_samples
            .fetch_add(block.zero_samples, Ordering::Relaxed);
        self.peak_bits
            .fetch_max(block.peak.to_bits(), Ordering::Relaxed);
    }

    fn snapshot(&self, channels: u16) -> InputLevel {
        InputLevel {
            channels,
            block: BlockLevel {
                samples: self.samples.load(Ordering::Relaxed),
                zero_samples: self.zero_samples.load(Ordering::Relaxed),
                peak: f32::from_bits(self.peak_bits.load(Ordering::Relaxed)),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct InputLevel {
    channels: u16,
    block: BlockLevel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    NoAudio,
    DigitalSilence,
    Sound,
}

impl InputLevel {
    fn frames(self) -> u64 {
        self.block.samples / u64::from(self.channels)
    }

    fn verdict(self) -> Verdict {
        match self.block {
            BlockLevel { samples: 0, .. } => Verdict::NoAudio,
            BlockLevel {
                samples,
                zero_samples,
                ..
            } if zero_samples == samples => Verdict::DigitalSilence,
            BlockLevel { .. } => Verdict::Sound,
        }
    }

    fn peak_dbfs(self) -> Dbfs {
        Dbfs(self.block.peak)
    }

    fn zero_share(self) -> ZeroShare {
        ZeroShare(self.block)
    }
}

struct Dbfs(f32);

impl fmt::Display for Dbfs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 > 0.0 {
            write!(f, "{:.1}", 20.0 * self.0.log10())
        } else {
            f.write_str("-inf")
        }
    }
}

struct ZeroShare(BlockLevel);

impl fmt::Display for ZeroShare {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let BlockLevel {
            samples,
            zero_samples,
            ..
        } = self.0;
        if samples == 0 {
            return f.write_str("none");
        }
        write!(f, "{:.1}%", 100.0 * zero_samples as f64 / samples as f64)
    }
}

fn report_first_seconds(level: InputLevel, requested: InputSource) -> RecordingSource {
    let source = RecordingSource::of_this_process();
    log_first_seconds(level, &source, &requested.with_default_named());
    source
}

fn log_first_seconds(level: InputLevel, source: &RecordingSource, requested: &InputSource) {
    let seconds = LEVEL_REPORT_DELAY.as_secs();
    match level.verdict() {
        Verdict::NoAudio => tracing::warn!(
            target: MICROPHONE_TARGET,
            %source,
            %requested,
            "AAudio: Roblox's microphone received no audio in its first {seconds} s"
        ),
        Verdict::DigitalSilence => tracing::warn!(
            target: MICROPHONE_TARGET,
            %source,
            %requested,
            frames = level.frames(),
            "{}",
            silence_advice(source, requested)
        ),
        Verdict::Sound => tracing::info!(
            target: MICROPHONE_TARGET,
            %source,
            %requested,
            frames = level.frames(),
            peak_dbfs = %level.peak_dbfs(),
            zero_share = %level.zero_share(),
            "AAudio: microphone level after {seconds} s"
        ),
    }
}

fn silence_advice(source: &RecordingSource, requested: &InputSource) -> &'static str {
    match (source, requested) {
        (RecordingSource::Listed(sources), InputSource::SystemDefault(Some(default)))
            if sources.iter().any(|source| source != default) =>
        {
            "AAudio: Roblox's microphone hears only silence from a source set for Eclipse in the \
             sound settings or pavucontrol: unmute it or choose one under Audio in Eclipse Settings"
        }
        _ => {
            "AAudio: Roblox's microphone hears only silence: unmute it (its mute button or the \
             system's sound settings) or choose another one under Audio in Eclipse Settings"
        }
    }
}

struct LevelReport {
    cancel: mpsc::Sender<()>,
    thread: JoinHandle<Option<RecordingSource>>,
}

impl LevelReport {
    fn start(
        delay: Duration,
        report: impl FnOnce() -> RecordingSource + Send + 'static,
    ) -> std::io::Result<Self> {
        let (cancel, cancelled) = mpsc::channel();
        let dispatch = tracing::dispatcher::get_default(tracing::Dispatch::clone);
        let thread = std::thread::Builder::new()
            .name(LEVEL_REPORT_THREAD.to_owned())
            .spawn(move || {
                let Err(RecvTimeoutError::Timeout) = cancelled.recv_timeout(delay) else {
                    return None;
                };
                Some(tracing::dispatcher::with_default(&dispatch, report))
            })?;
        Ok(Self { cancel, thread })
    }

    fn finish(self) -> Option<RecordingSource> {
        drop(self.cancel);
        self.thread
            .join()
            .expect("the microphone level report does not panic")
    }
}

struct Microphone {
    requested: InputSource,
    report: Option<LevelReport>,
}

struct StreamShared {
    state: AtomicStreamState,
    starts: AtomicU64,
    frames_per_burst: AtomicI32,
    xrun_count: AtomicI32,
    host_xrun: AtomicBool,
    host_callbacks: AtomicU64,
    levels: InputLevels,
}

impl StreamShared {
    fn new(frames_per_burst: u32) -> Self {
        Self {
            state: AtomicStreamState::new(StreamState::Open),
            starts: AtomicU64::new(0),
            frames_per_burst: AtomicI32::new(frames_per_burst as i32),
            xrun_count: AtomicI32::new(0),
            host_xrun: AtomicBool::new(false),
            host_callbacks: AtomicU64::new(0),
            levels: InputLevels::default(),
        }
    }

    fn capacity_frames(&self) -> i32 {
        2 * self.frames_per_burst.load(Ordering::Relaxed)
    }

    fn run(&self) -> Option<Run> {
        let starts = self.starts.load(Ordering::Acquire);
        (self.state.load() == StreamState::Started).then_some(Run(starts))
    }
}

struct RunTiming {
    run: Run,
    started: Instant,
    previous: Instant,
    ahead: Duration,
}

impl RunTiming {
    fn new(run: Run, at: Instant, delivered: Duration) -> Self {
        Self {
            run,
            started: at,
            previous: at,
            ahead: delivered,
        }
    }

    fn late(&mut self, at: Instant, delivered: Duration, buffer: Duration) -> bool {
        let gap = at.saturating_duration_since(self.previous);
        let late = gap > buffer.max(self.ahead) + buffer
            && at.saturating_duration_since(self.started) >= START_GRACE;
        self.ahead = if gap < self.ahead / 2 {
            self.ahead - gap + delivered
        } else {
            delivered
        };
        self.previous = at;
        late
    }
}

struct LateCallbackDetector {
    sample_rate: u32,
    buffer: Duration,
    timing: Option<RunTiming>,
}

impl LateCallbackDetector {
    fn new(capacity_frames: u32, sample_rate: u32) -> Self {
        Self {
            sample_rate,
            buffer: Duration::from_secs(u64::from(capacity_frames)) / sample_rate,
            timing: None,
        }
    }

    fn underrun(
        &mut self,
        at: Instant,
        frames: usize,
        run: Option<Run>,
        host_xrun: &AtomicBool,
    ) -> bool {
        let reported = host_xrun.swap(false, Ordering::Relaxed);
        let Some(run) = run else {
            self.timing = None;
            return false;
        };
        let delivered = Duration::from_secs(frames as u64) / self.sample_rate;
        let late = match &mut self.timing {
            Some(timing) if timing.run == run => timing.late(at, delivered, self.buffer),
            _ => {
                self.timing = Some(RunTiming::new(run, at, delivered));
                false
            }
        };
        reported || late
    }
}

#[derive(Default)]
struct LateCallbackLog {
    logged_xruns: i32,
    logged_at: Option<Instant>,
}

impl LateCallbackLog {
    fn due(&mut self, xruns: i32, at: Instant) -> bool {
        let recent = self.logged_at.is_some_and(|logged_at| {
            at.saturating_duration_since(logged_at) < LATE_CALLBACK_LOG_INTERVAL
        });
        if xruns == self.logged_xruns || recent {
            return false;
        }
        self.logged_xruns = xruns;
        self.logged_at = Some(at);
        true
    }
}

struct StreamEntry {
    shared: Arc<StreamShared>,
    direction: Direction,
    data_path: DataPath,
    format: StreamFormat,
    host: Option<cpal::Stream>,
    microphone: Option<Microphone>,
    read_logged: bool,
}

impl StreamEntry {
    fn report_levels_after_start(&mut self) {
        let Some(microphone) = &mut self.microphone else {
            return;
        };
        if microphone.report.is_some() {
            return;
        }
        let shared = Arc::clone(&self.shared);
        let channels = self.format.channels;
        let requested = microphone.requested.clone();
        match LevelReport::start(LEVEL_REPORT_DELAY, move || {
            report_first_seconds(shared.levels.snapshot(channels), requested)
        }) {
            Ok(report) => microphone.report = Some(report),
            Err(error) => tracing::warn!(
                target: "eclipse::audio",
                %error,
                "AAudio: the microphone level report cannot start"
            ),
        }
    }

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
            if next == StreamState::Started {
                self.shared.starts.fetch_add(1, Ordering::Release);
            }
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
    error_callback: Option<ErrorCallbackTarget>,
    route: Route,
    stream: usize,
    direction: Direction,
    channels: usize,
    scratch: Box<[A]>,
    late: LateCallbackDetector,
    late_log: LateCallbackLog,
}

impl<A: cpal::SizedSample> Renderer<A> {
    fn new(
        shared: Arc<StreamShared>,
        callback: DataCallbackTarget,
        error_callback: Option<ErrorCallbackTarget>,
        route: Route,
        stream: NdkHandle,
        direction: Direction,
        format: StreamFormat,
    ) -> Self {
        let channels = usize::from(format.channels);
        let capacity = u32::try_from(shared.capacity_frames()).unwrap_or(0).max(1);
        Self {
            shared,
            callback,
            error_callback,
            route,
            stream: stream as usize,
            direction,
            channels,
            scratch: vec![A::EQUILIBRIUM; capacity as usize * channels].into_boxed_slice(),
            late: LateCallbackDetector::new(capacity, format.sample_rate),
            late_log: LateCallbackLog::default(),
        }
    }

    fn follow_route(&self) {
        if !self.route.changed()
            || self.shared.state.swap(StreamState::Disconnected) == StreamState::Disconnected
        {
            return;
        }
        tracing::info!(
            target: "eclipse::audio",
            "AAudio: stream disconnected so that it reopens on the chosen device"
        );
        if let Some(target) = self.error_callback {
            target.report_disconnected(self.stream);
        }
    }

    fn note_burst(&self, frames: usize) {
        self.shared.host_callbacks.fetch_add(1, Ordering::Relaxed);
        let frames = frames as i32;
        if frames > 0 && self.shared.frames_per_burst.load(Ordering::Relaxed) != frames {
            self.shared
                .frames_per_burst
                .store(frames, Ordering::Relaxed);
        }
    }

    fn note_timing(&mut self, at: Instant, frames: usize) {
        if self
            .late
            .underrun(at, frames, self.shared.run(), &self.shared.host_xrun)
        {
            self.shared.xrun_count.fetch_add(1, Ordering::Relaxed);
        }
        let xruns = self.shared.xrun_count.load(Ordering::Relaxed);
        if self.late_log.due(xruns, at) {
            tracing::warn!(
                target: "eclipse::audio",
                direction = ?self.direction,
                xruns,
                "AAudio: late callbacks; audio likely dropped out"
            );
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

    fn render_output<T>(&mut self, out: &mut [T], at: Instant)
    where
        T: cpal::SizedSample + cpal::FromSample<A>,
    {
        self.follow_route();
        let frames = out.len() / self.channels;
        self.note_burst(frames);
        self.note_timing(at, frames);
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

    fn capture_input<T>(&mut self, input: &[T], at: Instant)
    where
        T: cpal::SizedSample,
        A: cpal::FromSample<T>,
        f32: cpal::FromSample<A>,
    {
        self.follow_route();
        let frames = input.len() / self.channels;
        self.note_burst(frames);
        self.note_timing(at, frames);
        let chunk_len = self.scratch.len();
        for chunk in input.chunks(chunk_len) {
            let mut level = BlockLevel::default();
            for (app, sample) in self.scratch.iter_mut().zip(chunk) {
                *app = A::from_sample(*sample);
                level.add(cpal::Sample::from_sample(*app));
            }
            if self.invoke(chunk.len()) {
                self.shared.levels.add(level);
            }
        }
    }
}

pub(crate) const HOST_ERROR_STREAK_LIMIT: Duration = Duration::from_millis(250);

#[derive(Clone, Copy)]
struct ErrorStreakStart {
    at: Instant,
    host_callbacks: u64,
}

#[derive(Default)]
pub(crate) struct HostErrorStreak(Option<ErrorStreakStart>);

impl HostErrorStreak {
    pub(crate) fn ends_stream(
        &mut self,
        kind: cpal::ErrorKind,
        host_callbacks: u64,
        at: Instant,
    ) -> bool {
        match kind {
            cpal::ErrorKind::Xrun
            | cpal::ErrorKind::DeviceChanged
            | cpal::ErrorKind::RealtimeDenied => false,
            cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::StreamInvalidated => true,
            _ => match self.0 {
                Some(start) if start.host_callbacks == host_callbacks => {
                    at.saturating_duration_since(start.at) >= HOST_ERROR_STREAK_LIMIT
                }
                _ => {
                    self.0 = Some(ErrorStreakStart { at, host_callbacks });
                    false
                }
            },
        }
    }
}

struct ErrorReporter {
    shared: Arc<StreamShared>,
    callback: Option<ErrorCallbackTarget>,
    stream: usize,
    streak: HostErrorStreak,
}

impl ErrorReporter {
    fn report(&mut self, error: cpal::Error, at: Instant) {
        if error.kind() == cpal::ErrorKind::Xrun {
            self.shared.host_xrun.store(true, Ordering::Relaxed);
        }
        let host_callbacks = self.shared.host_callbacks.load(Ordering::Relaxed);
        if !self.streak.ends_stream(error.kind(), host_callbacks, at)
            || self.shared.state.swap(StreamState::Disconnected) == StreamState::Disconnected
        {
            return;
        }
        tracing::warn!(target: "eclipse::audio", %error, "AAudio: stream disconnected");
        if let Some(target) = self.callback {
            target.report_disconnected(self.stream);
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
        streak: HostErrorStreak::default(),
    };
    let shared = Arc::clone(shared);
    let route = Route::of(plan.direction);
    match plan.format.app {
        SampleFormat::I16 => build_for_app::<i16>(
            device,
            plan,
            Renderer::new(
                shared,
                data_callback,
                params.error_callback,
                route,
                stream,
                plan.direction,
                plan.format,
            ),
            reporter,
        ),
        SampleFormat::Float => build_for_app::<f32>(
            device,
            plan,
            Renderer::new(
                shared,
                data_callback,
                params.error_callback,
                route,
                stream,
                plan.direction,
                plan.format,
            ),
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
    mut reporter: ErrorReporter,
) -> Result<cpal::Stream, cpal::Error>
where
    T: cpal::SizedSample + cpal::FromSample<A>,
    A: cpal::SizedSample + Send + 'static,
{
    device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            renderer.render_output(data, Instant::now())
        },
        move |error| reporter.report(error, Instant::now()),
        None,
    )
}

fn build_input<T, A>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut renderer: Renderer<A>,
    mut reporter: ErrorReporter,
) -> Result<cpal::Stream, cpal::Error>
where
    T: cpal::SizedSample,
    A: cpal::SizedSample + cpal::FromSample<T> + Send + 'static,
    f32: cpal::FromSample<A>,
{
    device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| renderer.capture_input(data, Instant::now()),
        move |error| reporter.report(error, Instant::now()),
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
    let device = crate::audio::host_device(params.direction).ok_or(AAUDIO_ERROR_UNAVAILABLE)?;
    let plan = plan_host_stream(&device, &params)?;
    let burst = match plan.config.buffer_size {
        cpal::BufferSize::Fixed(frames) => frames,
        cpal::BufferSize::Default => unreachable!("plan_host_stream always fixes the burst"),
    };
    let requested = match (params.direction, params.data_path) {
        (Direction::Input, DataPath::Callback(_)) => Some(InputSource::requested()),
        (Direction::Output, _) | (Direction::Input, DataPath::ReadWrite) => None,
    };
    let shared = Arc::new(StreamShared::new(burst));
    let handle = streams()
        .insert(StreamEntry {
            shared: Arc::clone(&shared),
            direction: params.direction,
            data_path: params.data_path,
            format: plan.format,
            host: None,
            microphone: requested.clone().map(|requested| Microphone {
                requested,
                report: None,
            }),
            read_logged: false,
        })
        .map_err(|_| AAUDIO_ERROR_NO_MEMORY)?;
    if let DataPath::Callback(data_callback) = params.data_path {
        attach_host_stream(&device, &plan, &params, data_callback, &shared, handle)?;
    }
    tracing::info!(
        target: "eclipse::audio",
        direction = ?params.direction,
        data_path = %params.data_path.label(),
        channels = plan.format.channels,
        sample_rate = plan.format.sample_rate,
        format = ?plan.format.app,
        frames_per_burst = burst,
        requested = requested.as_ref().map(tracing::field::display),
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
    stream_value(stream, |entry| {
        let code = entry.apply(request);
        if code == AAUDIO_OK && request == Request::Start {
            entry.report_levels_after_start();
        }
        code
    })
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
    let (host, microphone, shared, direction, channels) = match with_stream(stream, |entry| {
        (
            entry.host.take(),
            entry.microphone.take(),
            Arc::clone(&entry.shared),
            entry.direction,
            entry.format.channels,
        )
    }) {
        Ok(closing) => closing,
        Err(code) => return code,
    };
    drop(host);
    let level_report = microphone
        .and_then(|microphone| microphone.report)
        .map(LevelReport::finish);
    if streams().remove(handle_of(stream)).is_err() {
        return AAUDIO_ERROR_INVALID_HANDLE;
    }
    let xruns = shared.xrun_count.load(Ordering::Relaxed);
    match level_report {
        Some(source) => {
            let level = shared.levels.snapshot(channels);
            tracing::info!(
                target: "eclipse::audio",
                ?direction,
                xruns,
                source = source.as_ref().map(tracing::field::display),
                frames = level.frames(),
                peak_dbfs = %level.peak_dbfs(),
                zero_share = %level.zero_share(),
                "AAudio: stream closed"
            );
        }
        None => {
            tracing::info!(target: "eclipse::audio", ?direction, xruns, "AAudio: stream closed");
        }
    }
    AAUDIO_OK
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
    stream_value(stream, |entry| {
        let code = match (entry.direction, entry.data_path) {
            (Direction::Input, DataPath::Callback(_)) => AAUDIO_ERROR_INVALID_STATE,
            (Direction::Output, _) | (Direction::Input, DataPath::ReadWrite) => {
                AAUDIO_ERROR_UNIMPLEMENTED
            }
        };
        if !entry.read_logged {
            entry.read_logged = true;
            tracing::warn!(
                target: "eclipse::audio",
                direction = ?entry.direction,
                data_path = %entry.data_path.label(),
                code,
                "AAudio: read is not supported on this stream"
            );
        }
        code
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
