use std::f64::consts::TAU;
use std::ffi::{c_void, OsString};
use std::fmt;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use eclipse_config::audio::Direction;
use serde::Serialize;

use super::{
    builder_delete, builder_open_stream, builder_set_data_callback, builder_set_direction,
    builder_set_error_callback, builder_set_format, builder_set_input_preset,
    builder_set_performance_mode, builder_set_usage, create_stream_builder, stream_close,
    stream_get_channel_count, stream_get_format, stream_get_frames_per_burst,
    stream_get_sample_rate, stream_get_xrun_count, stream_request_start, stream_request_stop,
    DataCallback, AAUDIO_CALLBACK_RESULT_CONTINUE, AAUDIO_DIRECTION_INPUT, AAUDIO_DIRECTION_OUTPUT,
    AAUDIO_FORMAT_PCM_FLOAT, AAUDIO_OK, AAUDIO_PERFORMANCE_MODE_LOW_LATENCY,
};

pub const SINE_USAGE: &str =
    "usage: eclipse __audio-sine <seconds> <hz> [<stall_ms> <stall_every_s>]";

pub const LOOPBACK_USAGE: &str = "usage: eclipse __audio-loopback";

const AAUDIO_USAGE_GAME: i32 = 14;
const AAUDIO_INPUT_PRESET_VOICE_COMMUNICATION: i32 = 7;

const MAX_TONE_HZ: u32 = 20_000;
const TONE_AMPLITUDE: f64 = 0.5;

const CLICKS: usize = 10;
const CLICK_PERIOD: Duration = Duration::from_millis(500);
const CLICK_LENGTH: Duration = Duration::from_millis(5);
const CLICK_HZ: f64 = 1_000.0;
const CLICK_AMPLITUDE: f64 = 0.8;
const LEAD_IN: Duration = Duration::from_millis(500);
const CAPTURE_TONE_HZ: f64 = 997.0;
const CAPTURE_TONE: Duration = Duration::from_secs(1);
const TAIL: Duration = Duration::from_millis(500);
const DETECTION_LEVEL: f32 = 0.25;
const REFRACTORY: Duration = Duration::from_millis(100);
const PITCH_MARGIN: Duration = Duration::from_millis(100);
const PHASE_HOLD: Duration = Duration::from_millis(300);
const CAPTURE_SLACK: Duration = Duration::from_secs(2);
const SMALLEST_BURST_FRAMES: usize = 16;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn frames_in(duration: Duration, rate: u32) -> u64 {
    (duration.as_secs_f64() * f64::from(rate)).round() as u64
}

fn frame_count(frames: i32) -> usize {
    usize::try_from(frames).unwrap_or(0)
}

fn check(call: &str, code: i32) -> Result<(), String> {
    if code == AAUDIO_OK {
        Ok(())
    } else {
        Err(format!("{call} returned {code}"))
    }
}

fn json_line(value: &impl Serialize, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&serde_json::to_string(value).map_err(|_| fmt::Error)?)
}

#[derive(Default)]
struct StreamErrors(AtomicU32);

unsafe extern "C" fn count_stream_error(_stream: *mut c_void, user_data: *mut c_void, _error: i32) {
    let errors = unsafe { &*user_data.cast::<StreamErrors>() };
    errors.0.fetch_add(1, Ordering::Relaxed);
}

struct OpenStream(*mut c_void);

impl OpenStream {
    fn open(
        direction: Direction,
        data: Option<(DataCallback, *const c_void)>,
        errors: &StreamErrors,
    ) -> Result<Self, String> {
        let mut builder = std::ptr::null_mut();
        check("AAudio_createStreamBuilder", unsafe {
            create_stream_builder(&mut builder)
        })?;
        unsafe {
            match direction {
                Direction::Output => {
                    builder_set_direction(builder, AAUDIO_DIRECTION_OUTPUT);
                    builder_set_usage(builder, AAUDIO_USAGE_GAME);
                }
                Direction::Input => {
                    builder_set_direction(builder, AAUDIO_DIRECTION_INPUT);
                    builder_set_input_preset(builder, AAUDIO_INPUT_PRESET_VOICE_COMMUNICATION);
                }
            }
            builder_set_performance_mode(builder, AAUDIO_PERFORMANCE_MODE_LOW_LATENCY);
            builder_set_format(builder, AAUDIO_FORMAT_PCM_FLOAT);
            if let Some((callback, user_data)) = data {
                builder_set_data_callback(builder, Some(callback), user_data.cast_mut());
            }
            builder_set_error_callback(
                builder,
                Some(count_stream_error),
                std::ptr::from_ref(errors).cast_mut().cast(),
            );
        }
        let mut stream = std::ptr::null_mut();
        let opened = unsafe { builder_open_stream(builder, &mut stream) };
        unsafe { builder_delete(builder) };
        check("AAudioStreamBuilder_openStream", opened)?;
        let stream = Self(stream);
        match stream.query(stream_get_format) {
            AAUDIO_FORMAT_PCM_FLOAT => Ok(stream),
            format => Err(format!(
                "the {direction:?} stream opened with format {format}, not float"
            )),
        }
    }

    fn query(&self, getter: unsafe extern "C" fn(*mut c_void) -> i32) -> i32 {
        unsafe { getter(self.0) }
    }

    fn layout(&self) -> Result<(u32, usize), String> {
        let rate = self.query(stream_get_sample_rate);
        let channels = self.query(stream_get_channel_count);
        match (u32::try_from(rate), usize::try_from(channels)) {
            (Ok(rate @ 1..), Ok(channels @ 1..)) => Ok((rate, channels)),
            _ => Err(format!(
                "the stream reports {rate} Hz and {channels} channels"
            )),
        }
    }

    fn start(&self) -> Result<(), String> {
        check("AAudioStream_requestStart", unsafe {
            stream_request_start(self.0)
        })
    }

    fn stop(&self) -> Result<(), String> {
        check("AAudioStream_requestStop", unsafe {
            stream_request_stop(self.0)
        })
    }

    fn close(self) -> Result<(), String> {
        let code = unsafe { stream_close(self.0) };
        std::mem::forget(self);
        check("AAudioStream_close", code)
    }
}

impl Drop for OpenStream {
    fn drop(&mut self) {
        unsafe { stream_close(self.0) };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stall {
    length: Duration,
    every: Duration,
}

impl Stall {
    fn due(self, next: &mut Option<Instant>, now: Instant) -> bool {
        let due = *next.get_or_insert(now + self.every);
        if now < due {
            return false;
        }
        *next = Some(due + self.every);
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SineRequest {
    length: Duration,
    hz: u32,
    stall: Option<Stall>,
}

impl SineRequest {
    pub fn parse(arguments: &[OsString]) -> Result<Self, String> {
        let numbers = arguments
            .iter()
            .map(|argument| {
                argument
                    .to_str()
                    .and_then(|text| text.parse::<u32>().ok())
                    .filter(|number| *number > 0)
            })
            .collect::<Option<Vec<u32>>>()
            .ok_or(SINE_USAGE)?;
        let (seconds, hz, stall) = match numbers[..] {
            [seconds, hz] => (seconds, hz, None),
            [seconds, hz, stall_ms, stall_every_s] => (
                seconds,
                hz,
                Some(Stall {
                    length: Duration::from_millis(u64::from(stall_ms)),
                    every: Duration::from_secs(u64::from(stall_every_s)),
                }),
            ),
            _ => return Err(SINE_USAGE.to_owned()),
        };
        if hz > MAX_TONE_HZ {
            return Err(format!("{hz} Hz is above {MAX_TONE_HZ} Hz\n{SINE_USAGE}"));
        }
        Ok(Self {
            length: Duration::from_secs(u64::from(seconds)),
            hz,
            stall,
        })
    }
}

#[derive(Debug)]
struct Oscillator {
    phase: f64,
    step: f64,
    channels: usize,
}

impl Oscillator {
    fn new(hz: f64, rate: u32, channels: usize) -> Self {
        Self {
            phase: 0.0,
            step: TAU * hz / f64::from(rate),
            channels,
        }
    }

    fn fill(&mut self, samples: &mut [f32]) {
        for frame in samples.chunks_exact_mut(self.channels) {
            frame.fill((TONE_AMPLITUDE * self.phase.sin()) as f32);
            self.phase = (self.phase + self.step) % TAU;
        }
    }
}

struct ToneState {
    oscillator: Oscillator,
    next_stall: Option<Instant>,
}

struct Tone {
    state: Mutex<Option<ToneState>>,
    stall: Option<Stall>,
    callbacks: AtomicU64,
    stalls: AtomicU32,
}

impl Tone {
    fn new(stall: Option<Stall>) -> Self {
        Self {
            state: Mutex::new(None),
            stall,
            callbacks: AtomicU64::new(0),
            stalls: AtomicU32::new(0),
        }
    }
}

unsafe extern "C" fn play_tone(
    _stream: *mut c_void,
    user_data: *mut c_void,
    audio: *mut c_void,
    frames: i32,
) -> i32 {
    let tone = unsafe { &*user_data.cast::<Tone>() };
    tone.callbacks.fetch_add(1, Ordering::Relaxed);
    let mut state = lock(&tone.state);
    let Some(state) = state.as_mut() else {
        return AAUDIO_CALLBACK_RESULT_CONTINUE;
    };
    let samples = unsafe {
        std::slice::from_raw_parts_mut(
            audio.cast::<f32>(),
            frame_count(frames) * state.oscillator.channels,
        )
    };
    state.oscillator.fill(samples);
    if let Some(stall) = tone.stall {
        if stall.due(&mut state.next_stall, Instant::now()) {
            std::thread::sleep(stall.length);
            tone.stalls.fetch_add(1, Ordering::Relaxed);
        }
    }
    AAUDIO_CALLBACK_RESULT_CONTINUE
}

#[derive(Debug, Serialize)]
pub struct SineReport {
    sample_rate: u32,
    channels: usize,
    frames_per_burst: i32,
    callbacks: u64,
    xruns: i32,
    stalls: u32,
    stream_errors: u32,
}

impl fmt::Display for SineReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        json_line(self, f)
    }
}

pub fn run_sine(request: SineRequest) -> Result<SineReport, String> {
    let errors = StreamErrors::default();
    let tone = Tone::new(request.stall);
    let stream = OpenStream::open(
        Direction::Output,
        Some((play_tone, std::ptr::from_ref(&tone).cast())),
        &errors,
    )?;
    let (sample_rate, channels) = stream.layout()?;
    *lock(&tone.state) = Some(ToneState {
        oscillator: Oscillator::new(f64::from(request.hz), sample_rate, channels),
        next_stall: None,
    });
    stream.start()?;
    std::thread::sleep(request.length);
    stream.stop()?;
    let frames_per_burst = stream.query(stream_get_frames_per_burst);
    let xruns = stream.query(stream_get_xrun_count);
    stream.close()?;
    Ok(SineReport {
        sample_rate,
        channels,
        frames_per_burst,
        callbacks: tone.callbacks.load(Ordering::Relaxed),
        xruns,
        stalls: tone.stalls.load(Ordering::Relaxed),
        stream_errors: errors.0.load(Ordering::Relaxed),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cue {
    Click { index: usize, offset: u64 },
    Tone { offset: u64 },
    Silence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Schedule {
    rate: u32,
    lead: u64,
    period: u64,
    click: u64,
    tone_start: u64,
    tone: u64,
}

impl Schedule {
    fn at(rate: u32) -> Self {
        let lead = frames_in(LEAD_IN, rate);
        let period = frames_in(CLICK_PERIOD, rate);
        Self {
            rate,
            lead,
            period,
            click: frames_in(CLICK_LENGTH, rate),
            tone_start: lead + (CLICKS as u64 + 1) * period,
            tone: frames_in(CAPTURE_TONE, rate),
        }
    }

    fn length(&self) -> Duration {
        Duration::from_secs_f64((self.tone_start + self.tone) as f64 / f64::from(self.rate)) + TAIL
    }

    fn cue(&self, frame: u64) -> Cue {
        if let Some(offset) = frame
            .checked_sub(self.tone_start)
            .filter(|offset| *offset < self.tone)
        {
            return Cue::Tone { offset };
        }
        let Some(since) = frame.checked_sub(self.lead) else {
            return Cue::Silence;
        };
        let index = usize::try_from(since / self.period).unwrap_or(usize::MAX);
        let offset = since % self.period;
        if index < CLICKS && offset < self.click {
            Cue::Click { index, offset }
        } else {
            Cue::Silence
        }
    }
}

#[derive(Debug)]
struct ClickTrack {
    schedule: Schedule,
    channels: usize,
    frame: u64,
    clicks: [Option<Instant>; CLICKS],
    tone: Option<Instant>,
}

impl ClickTrack {
    fn new(schedule: Schedule, channels: usize) -> Self {
        Self {
            schedule,
            channels,
            frame: 0,
            clicks: [None; CLICKS],
            tone: None,
        }
    }

    fn render(&mut self, samples: &mut [f32], now: Instant) {
        let rate = f64::from(self.schedule.rate);
        let mut position = 0_u64;
        for frame in samples.chunks_exact_mut(self.channels) {
            let at = now + Duration::from_secs_f64(position as f64 / rate);
            let value = match self.schedule.cue(self.frame + position) {
                Cue::Click { index, offset } => {
                    if offset == 0 {
                        self.clicks[index] = Some(at);
                    }
                    CLICK_AMPLITUDE * (TAU * CLICK_HZ * offset as f64 / rate).sin()
                }
                Cue::Tone { offset } => {
                    if offset == 0 {
                        self.tone = Some(at);
                    }
                    TONE_AMPLITUDE * (TAU * CAPTURE_TONE_HZ * offset as f64 / rate).sin()
                }
                Cue::Silence => 0.0,
            };
            frame.fill(value as f32);
            position += 1;
        }
        self.frame += position;
    }
}

#[derive(Clone, Copy, Debug)]
struct CaptureMark {
    at: Instant,
    end: usize,
}

#[derive(Debug)]
struct Capture {
    rate: u32,
    channels: usize,
    samples: Vec<f32>,
    marks: Vec<CaptureMark>,
}

impl Capture {
    fn new(rate: u32, channels: usize, length: Duration) -> Self {
        let frames = usize::try_from(frames_in(length, rate)).unwrap_or(usize::MAX);
        Self {
            rate,
            channels,
            samples: Vec::with_capacity(frames),
            marks: Vec::with_capacity(frames / SMALLEST_BURST_FRAMES),
        }
    }

    fn record(&mut self, input: &[f32], at: Instant) {
        let room = self.samples.capacity() - self.samples.len();
        self.samples.extend(
            input
                .chunks_exact(self.channels)
                .map(|frame| frame[0])
                .take(room),
        );
        if self.marks.len() < self.marks.capacity() {
            self.marks.push(CaptureMark {
                at,
                end: self.samples.len(),
            });
        }
    }

    fn instant_of(&self, frame: usize) -> Option<Instant> {
        let mark = self
            .marks
            .get(self.marks.partition_point(|mark| mark.end <= frame))?;
        let earlier = (mark.end - 1 - frame) as f64 / f64::from(self.rate);
        mark.at.checked_sub(Duration::from_secs_f64(earlier))
    }
}

fn onsets(samples: &[f32], level: f32, refractory: usize) -> Vec<usize> {
    let mut found = Vec::new();
    let mut ready_at = 0;
    for (index, sample) in samples.iter().enumerate() {
        if index >= ready_at && sample.abs() > level {
            found.push(index);
            ready_at = index.saturating_add(refractory);
        }
    }
    found
}

fn round_trips(sent: &[Option<Instant>], heard: &[Instant], window: Duration) -> Vec<Duration> {
    sent.iter()
        .flatten()
        .filter_map(|&sent| {
            heard
                .iter()
                .filter_map(|heard| heard.checked_duration_since(sent))
                .find(|delay| *delay < window)
        })
        .collect()
}

fn zero_crossing_hz(samples: &[f32], rate: u32) -> Option<f64> {
    let mut first = None;
    let mut last = 0.0;
    let mut crossings = 0_u32;
    for (index, pair) in samples.windows(2).enumerate() {
        let (before, after) = (f64::from(pair[0]), f64::from(pair[1]));
        if before < 0.0 && after >= 0.0 {
            last = index as f64 + before / (before - after);
            first.get_or_insert(last);
            crossings += 1;
        }
    }
    let first = first?;
    (crossings >= 2 && last > first)
        .then(|| f64::from(crossings - 1) * f64::from(rate) / (last - first))
}

fn percentile_ms(sorted: &[Duration], share: f64) -> Option<f64> {
    let rank = ((sorted.len() as f64 * share).ceil() as usize).max(1);
    sorted
        .get(rank - 1)
        .map(|delay| delay.as_secs_f64() * 1_000.0)
}

#[derive(Debug, PartialEq)]
struct Heard {
    round_trips: Vec<Duration>,
    capture_hz: Option<f64>,
}

fn listen(track: &ClickTrack, capture: &Capture) -> Heard {
    let refractory = usize::try_from(frames_in(REFRACTORY, capture.rate)).unwrap_or(usize::MAX);
    let found: Vec<(usize, Instant)> = onsets(&capture.samples, DETECTION_LEVEL, refractory)
        .into_iter()
        .filter_map(|frame| Some((frame, capture.instant_of(frame)?)))
        .collect();
    let heard: Vec<Instant> = found.iter().map(|(_, at)| *at).collect();
    let mut round_trips = round_trips(&track.clicks, &heard, CLICK_PERIOD);
    round_trips.sort_unstable();
    let capture_hz = track.tone.and_then(|sent| {
        let (onset, _) = found.iter().find(|(_, at)| *at >= sent)?;
        let margin = usize::try_from(frames_in(PITCH_MARGIN, capture.rate)).ok()?;
        let tone = usize::try_from(frames_in(CAPTURE_TONE, capture.rate)).ok()?;
        let tone = capture
            .samples
            .get(onset + margin..(onset + tone - margin).min(capture.samples.len()))?;
        zero_crossing_hz(tone, capture.rate)
    });
    Heard {
        round_trips,
        capture_hz,
    }
}

#[derive(Default)]
struct Loopback {
    track: Mutex<Option<ClickTrack>>,
    capture: Mutex<Option<Capture>>,
}

unsafe extern "C" fn play_clicks(
    _stream: *mut c_void,
    user_data: *mut c_void,
    audio: *mut c_void,
    frames: i32,
) -> i32 {
    let now = Instant::now();
    let loopback = unsafe { &*user_data.cast::<Loopback>() };
    if let Some(track) = lock(&loopback.track).as_mut() {
        let samples = unsafe {
            std::slice::from_raw_parts_mut(
                audio.cast::<f32>(),
                frame_count(frames) * track.channels,
            )
        };
        track.render(samples, now);
    }
    AAUDIO_CALLBACK_RESULT_CONTINUE
}

unsafe extern "C" fn record_input(
    _stream: *mut c_void,
    user_data: *mut c_void,
    audio: *mut c_void,
    frames: i32,
) -> i32 {
    let now = Instant::now();
    let loopback = unsafe { &*user_data.cast::<Loopback>() };
    if let Some(capture) = lock(&loopback.capture).as_mut() {
        let input = unsafe {
            std::slice::from_raw_parts(audio.cast::<f32>(), frame_count(frames) * capture.channels)
        };
        capture.record(input, now);
    }
    AAUDIO_CALLBACK_RESULT_CONTINUE
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Probe,
    Opened,
    Started,
    Stopped,
    Closed,
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[derive(Serialize)]
        struct Line {
            phase: Phase,
        }
        json_line(&Line { phase: *self }, f)
    }
}

#[derive(Debug, Serialize)]
pub struct LoopbackReport {
    output_rate: u32,
    output_channels: usize,
    input_rate: u32,
    input_channels: usize,
    clicks: usize,
    detected: usize,
    round_trip_ms_p50: Option<f64>,
    round_trip_ms_p95: Option<f64>,
    tone_hz: f64,
    capture_hz: Option<f64>,
    stream_errors: u32,
}

impl fmt::Display for LoopbackReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        json_line(self, f)
    }
}

fn hold(phase: Phase, on_phase: &mut impl FnMut(Phase)) {
    on_phase(phase);
    std::thread::sleep(PHASE_HOLD);
}

pub fn run_loopback(mut on_phase: impl FnMut(Phase)) -> Result<LoopbackReport, String> {
    let errors = StreamErrors::default();
    let probe = OpenStream::open(Direction::Input, None, &errors)?;
    hold(Phase::Probe, &mut on_phase);
    probe.close()?;

    let loopback = Loopback::default();
    let user_data = std::ptr::from_ref(&loopback).cast::<c_void>();
    let input = OpenStream::open(Direction::Input, Some((record_input, user_data)), &errors)?;
    let output = OpenStream::open(Direction::Output, Some((play_clicks, user_data)), &errors)?;
    let (input_rate, input_channels) = input.layout()?;
    let (output_rate, output_channels) = output.layout()?;
    let schedule = Schedule::at(output_rate);
    *lock(&loopback.capture) = Some(Capture::new(
        input_rate,
        input_channels,
        schedule.length() + CAPTURE_SLACK,
    ));
    *lock(&loopback.track) = Some(ClickTrack::new(schedule, output_channels));
    hold(Phase::Opened, &mut on_phase);

    input.start()?;
    output.start()?;
    on_phase(Phase::Started);
    std::thread::sleep(schedule.length());
    output.stop()?;
    input.stop()?;
    hold(Phase::Stopped, &mut on_phase);
    output.close()?;
    input.close()?;
    hold(Phase::Closed, &mut on_phase);

    let track = lock(&loopback.track)
        .take()
        .ok_or("the click track vanished")?;
    let capture = lock(&loopback.capture)
        .take()
        .ok_or("the capture vanished")?;
    let heard = listen(&track, &capture);
    Ok(LoopbackReport {
        output_rate,
        output_channels,
        input_rate,
        input_channels,
        clicks: CLICKS,
        detected: heard.round_trips.len(),
        round_trip_ms_p50: percentile_ms(&heard.round_trips, 0.5),
        round_trip_ms_p95: percentile_ms(&heard.round_trips, 0.95),
        tone_hz: CAPTURE_TONE_HZ,
        capture_hz: heard.capture_hz,
        stream_errors: errors.0.load(Ordering::Relaxed),
    })
}

#[cfg(test)]
mod tests;
