use super::*;
use cpal::traits::HostTrait;
use std::ffi::{c_char, c_int, CStr, OsStr};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use eclipse_config::audio::AudioDevice;

use crate::audio::null_devices::{stream_device, NullDevices};
use crate::audio::private_server::{assert_passed, on_private_server, outcome, test_command};
use crate::audio::Routing;
use crate::loader::log_capture::formatted_log;
use crate::loader::native_provider::EclipseNativeProvider;
use crate::loader::resolve::SymbolProvider;

fn range(channels: u16, format: cpal::SampleFormat) -> cpal::SupportedStreamConfigRange {
    cpal::SupportedStreamConfigRange::new(
        channels,
        8_000,
        384_000,
        cpal::SupportedBufferSize::Unknown,
        format,
    )
}

fn device_default(
    channels: u16,
    sample_rate: u32,
    format: cpal::SampleFormat,
) -> cpal::SupportedStreamConfig {
    cpal::SupportedStreamConfig::new(
        channels,
        sample_rate,
        cpal::SupportedBufferSize::Unknown,
        format,
    )
}

fn detached_stream(direction: Direction) -> (*mut c_void, Arc<StreamShared>) {
    let callback = DataPath::Callback(DataCallbackTarget {
        func: write_silence,
        user_data: 0,
    });
    detached_stream_with(direction, callback)
}

fn detached_stream_with(
    direction: Direction,
    data_path: DataPath,
) -> (*mut c_void, Arc<StreamShared>) {
    let shared = Arc::new(StreamShared::new(256));
    let handle = streams()
        .insert(StreamEntry {
            shared: Arc::clone(&shared),
            direction,
            data_path,
            format: StreamFormat {
                channels: 2,
                sample_rate: 48_000,
                app: SampleFormat::Float,
                device: DeviceFormat::Float,
            },
            host: None,
            read_logged: false,
        })
        .expect("insert stream");
    (ptr_of(handle), shared)
}

#[derive(Default)]
struct CallbackProbe {
    calls: AtomicUsize,
    frames: Mutex<Vec<i32>>,
    received: Mutex<Vec<i16>>,
    result: AtomicI32,
    collision_results: Mutex<Vec<i32>>,
    errors: Mutex<Vec<i32>>,
    bytes_per_frame: AtomicUsize,
}

impl CallbackProbe {
    fn user_data(&self) -> usize {
        std::ptr::from_ref(self) as usize
    }
}

unsafe extern "C" fn write_i16_ramp(
    _stream: *mut c_void,
    user_data: *mut c_void,
    audio: *mut c_void,
    frames: i32,
) -> i32 {
    let probe = unsafe { &*(user_data as *const CallbackProbe) };
    probe.calls.fetch_add(1, Ordering::SeqCst);
    probe.frames.lock().unwrap().push(frames);
    let samples =
        unsafe { std::slice::from_raw_parts_mut(audio.cast::<i16>(), frames as usize * 2) };
    for (i, sample) in samples.iter_mut().enumerate() {
        *sample = (i as i16) * 1000;
    }
    probe.result.load(Ordering::SeqCst)
}

unsafe extern "C" fn record_i16_input(
    _stream: *mut c_void,
    user_data: *mut c_void,
    audio: *mut c_void,
    frames: i32,
) -> i32 {
    let probe = unsafe { &*(user_data as *const CallbackProbe) };
    probe.calls.fetch_add(1, Ordering::SeqCst);
    let samples = unsafe { std::slice::from_raw_parts(audio.cast::<i16>(), frames as usize * 2) };
    probe.received.lock().unwrap().extend_from_slice(samples);
    AAUDIO_CALLBACK_RESULT_CONTINUE
}

unsafe extern "C" fn control_from_callback(
    stream: *mut c_void,
    user_data: *mut c_void,
    _audio: *mut c_void,
    _frames: i32,
) -> i32 {
    let probe = unsafe { &*(user_data as *const CallbackProbe) };
    probe.calls.fetch_add(1, Ordering::SeqCst);
    let results = unsafe {
        [
            stream_request_stop(stream),
            stream_request_pause(stream),
            stream_request_start(stream),
            stream_close(stream),
        ]
    };
    probe.collision_results.lock().unwrap().extend(results);
    AAUDIO_CALLBACK_RESULT_CONTINUE
}

unsafe extern "C" fn write_silence(
    _stream: *mut c_void,
    user_data: *mut c_void,
    audio: *mut c_void,
    frames: i32,
) -> i32 {
    let probe = unsafe { &*(user_data as *const CallbackProbe) };
    let bytes = frames as usize * probe.bytes_per_frame.load(Ordering::SeqCst);
    unsafe { std::ptr::write_bytes(audio.cast::<u8>(), 0, bytes) };
    probe.calls.fetch_add(1, Ordering::SeqCst);
    AAUDIO_CALLBACK_RESULT_CONTINUE
}

unsafe extern "C" fn record_error(_stream: *mut c_void, user_data: *mut c_void, error: i32) {
    let probe = unsafe { &*(user_data as *const CallbackProbe) };
    probe.errors.lock().unwrap().push(error);
}

static UNCHANGED_ROUTE: AtomicU64 = AtomicU64::new(0);

fn renderer<A: cpal::SizedSample>(
    shared: &Arc<StreamShared>,
    callback: DataCallback,
    probe: &CallbackProbe,
) -> Renderer<A> {
    routed_renderer(shared, callback, probe, Route::on(&UNCHANGED_ROUTE))
}

fn routed_renderer<A: cpal::SizedSample>(
    shared: &Arc<StreamShared>,
    callback: DataCallback,
    probe: &CallbackProbe,
    route: Route,
) -> Renderer<A> {
    Renderer::new(
        Arc::clone(shared),
        DataCallbackTarget {
            func: callback,
            user_data: probe.user_data(),
        },
        Some(ErrorCallbackTarget {
            func: record_error,
            user_data: probe.user_data(),
        }),
        route,
        0x1234_0000_0007,
        Direction::Output,
        StreamFormat {
            channels: 2,
            sample_rate: 48_000,
            app: SampleFormat::I16,
            device: DeviceFormat::Float,
        },
    )
}

#[test]
fn register_natives_count_matches() {
    let mut names = Vec::new();
    register_natives(|name, addr| {
        assert_ne!(addr, 0, "{name} has an implementation");
        names.push(name);
    });
    assert_eq!(names.len(), AAUDIO_NATIVE_COUNT);
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), AAUDIO_NATIVE_COUNT, "names are unique");
}

#[test]
fn unspecified_format_takes_the_device_default_format_at_the_app_rate_and_channels() {
    let format = negotiate_format(
        None,
        &device_default(2, 44_100, cpal::SampleFormat::F32),
        [
            range(2, cpal::SampleFormat::F32),
            range(2, cpal::SampleFormat::I16),
            range(1, cpal::SampleFormat::F32),
        ],
    );
    assert_eq!(
        format,
        Ok(StreamFormat {
            channels: 2,
            sample_rate: 48_000,
            app: SampleFormat::Float,
            device: DeviceFormat::Float,
        })
    );
}

#[test]
fn a_mono_16_khz_default_device_still_opens_at_48_khz_stereo() {
    let format = negotiate_format(
        None,
        &device_default(1, 16_000, cpal::SampleFormat::F32),
        [
            range(1, cpal::SampleFormat::F32),
            range(2, cpal::SampleFormat::F32),
        ],
    );
    assert_eq!(
        format,
        Ok(StreamFormat {
            channels: 2,
            sample_rate: 48_000,
            app: SampleFormat::Float,
            device: DeviceFormat::Float,
        })
    );
}

#[test]
fn a_device_without_48_khz_stereo_keeps_its_default_rate_and_channels() {
    let cd_only = cpal::SupportedStreamConfigRange::new(
        2,
        44_100,
        44_100,
        cpal::SupportedBufferSize::Unknown,
        cpal::SampleFormat::F32,
    );
    let format = negotiate_format(
        None,
        &device_default(2, 44_100, cpal::SampleFormat::F32),
        [cd_only],
    );
    assert_eq!(
        format,
        Ok(StreamFormat {
            channels: 2,
            sample_rate: 44_100,
            app: SampleFormat::Float,
            device: DeviceFormat::Float,
        })
    );
}

#[test]
fn a_48_khz_stereo_range_without_a_convertible_format_falls_back_to_the_device_default() {
    let format = negotiate_format(
        None,
        &device_default(1, 44_100, cpal::SampleFormat::F32),
        [
            range(2, cpal::SampleFormat::U8),
            range(1, cpal::SampleFormat::F32),
        ],
    );
    assert_eq!(
        format,
        Ok(StreamFormat {
            channels: 1,
            sample_rate: 44_100,
            app: SampleFormat::Float,
            device: DeviceFormat::Float,
        })
    );
}

#[test]
fn requested_i16_is_honoured_natively_when_the_device_supports_it() {
    let format = negotiate_format(
        Some(SampleFormat::I16),
        &device_default(2, 44_100, cpal::SampleFormat::F32),
        [
            range(2, cpal::SampleFormat::F32),
            range(2, cpal::SampleFormat::I16),
        ],
    )
    .unwrap();
    assert_eq!(
        (format.app, format.device),
        (SampleFormat::I16, DeviceFormat::I16)
    );
    assert_eq!((format.channels, format.sample_rate), (2, 48_000));
}

#[test]
fn requested_format_is_converted_when_the_device_lacks_it() {
    let format = negotiate_format(
        Some(SampleFormat::I16),
        &device_default(2, 48_000, cpal::SampleFormat::F32),
        [range(2, cpal::SampleFormat::F32)],
    )
    .unwrap();
    assert_eq!(
        format.app,
        SampleFormat::I16,
        "the app sees what it asked for"
    );
    assert_eq!(
        format.device,
        DeviceFormat::Float,
        "the device gets what it supports"
    );
}

#[test]
fn i16_only_device_defaults_the_app_to_i16() {
    let format = negotiate_format(
        None,
        &device_default(1, 22_050, cpal::SampleFormat::I16),
        [range(1, cpal::SampleFormat::I16)],
    )
    .unwrap();
    assert_eq!(
        format,
        StreamFormat {
            channels: 1,
            sample_rate: 22_050,
            app: SampleFormat::I16,
            device: DeviceFormat::I16,
        }
    );
}

#[test]
fn i32_default_device_gets_float_app_samples_at_full_precision() {
    let format = negotiate_format(
        None,
        &device_default(2, 48_000, cpal::SampleFormat::I32),
        [
            range(2, cpal::SampleFormat::I32),
            range(2, cpal::SampleFormat::I16),
        ],
    )
    .unwrap();
    assert_eq!(
        (format.app, format.device),
        (SampleFormat::Float, DeviceFormat::I32)
    );
}

#[test]
fn i24_only_device_converts_the_requested_app_format() {
    let format = negotiate_format(
        Some(SampleFormat::I16),
        &device_default(2, 48_000, cpal::SampleFormat::I24),
        [range(2, cpal::SampleFormat::I24)],
    )
    .unwrap();
    assert_eq!(
        (format.app, format.device),
        (SampleFormat::I16, DeviceFormat::I24)
    );
}

#[test]
fn device_without_a_convertible_format_at_its_default_config_is_unavailable() {
    let at_other_rate = cpal::SupportedStreamConfigRange::new(
        2,
        8_000,
        16_000,
        cpal::SupportedBufferSize::Unknown,
        cpal::SampleFormat::F32,
    );
    assert_eq!(
        negotiate_format(
            None,
            &device_default(2, 48_000, cpal::SampleFormat::U8),
            [range(2, cpal::SampleFormat::U8), at_other_rate],
        ),
        Err(AAUDIO_ERROR_UNAVAILABLE)
    );
}

#[test]
fn burst_follows_performance_mode_and_device_limits() {
    let unknown = cpal::SupportedBufferSize::Unknown;
    assert_eq!(
        burst_frames(48_000, PerformanceMode::LowLatency, &unknown),
        512
    );
    assert_eq!(
        burst_frames(44_100, PerformanceMode::LowLatency, &unknown),
        512
    );
    assert_eq!(burst_frames(48_000, PerformanceMode::None, &unknown), 1024);
    assert_eq!(
        burst_frames(48_000, PerformanceMode::PowerSaving, &unknown),
        2048
    );
    let limited = cpal::SupportedBufferSize::Range {
        min: 1024,
        max: 4096,
    };
    assert_eq!(
        burst_frames(48_000, PerformanceMode::LowLatency, &limited),
        1024
    );
}

#[test]
fn builder_parameters_are_validated_at_open() {
    unsafe extern "C" fn cb(_: *mut c_void, _: *mut c_void, _: *mut c_void, _: i32) -> i32 {
        AAUDIO_CALLBACK_RESULT_CONTINUE
    }
    let valid = BuilderSettings {
        data_callback: Some(DataCallbackTarget {
            func: cb,
            user_data: 0,
        }),
        ..BuilderSettings::default()
    };
    let parsed = StreamParameters::parse(&valid).unwrap();
    assert_eq!(parsed.direction, Direction::Output);
    assert_eq!(parsed.format, None);
    assert_eq!(parsed.performance_mode, PerformanceMode::None);
    assert!(matches!(parsed.data_path, DataPath::Callback(_)));

    let without_callback = BuilderSettings {
        data_callback: None,
        ..valid
    };
    assert!(matches!(
        StreamParameters::parse(&without_callback).map(|parsed| parsed.data_path),
        Ok(DataPath::ReadWrite)
    ));

    let cases = [
        (
            BuilderSettings {
                direction: 2,
                ..valid
            },
            AAUDIO_ERROR_ILLEGAL_ARGUMENT,
        ),
        (
            BuilderSettings { format: 3, ..valid },
            AAUDIO_ERROR_INVALID_FORMAT,
        ),
        (
            BuilderSettings {
                performance_mode: 13,
                ..valid
            },
            AAUDIO_ERROR_ILLEGAL_ARGUMENT,
        ),
        (
            BuilderSettings { usage: 15, ..valid },
            AAUDIO_ERROR_ILLEGAL_ARGUMENT,
        ),
        (
            BuilderSettings {
                input_preset: 2,
                ..valid
            },
            AAUDIO_ERROR_ILLEGAL_ARGUMENT,
        ),
        (
            BuilderSettings {
                buffer_capacity: -1,
                ..valid
            },
            AAUDIO_ERROR_OUT_OF_RANGE,
        ),
    ];
    for (settings, expected) in cases {
        assert_eq!(StreamParameters::parse(&settings).err(), Some(expected));
    }

    let fmod = BuilderSettings {
        direction: AAUDIO_DIRECTION_INPUT,
        format: AAUDIO_FORMAT_PCM_I16,
        performance_mode: AAUDIO_PERFORMANCE_MODE_LOW_LATENCY,
        usage: 14,
        input_preset: 7,
        buffer_capacity: 4096,
        ..valid
    };
    let parsed = StreamParameters::parse(&fmod).unwrap();
    assert_eq!(parsed.direction, Direction::Input);
    assert_eq!(parsed.format, Some(SampleFormat::I16));
    assert_eq!(parsed.performance_mode, PerformanceMode::LowLatency);
}

#[test]
fn request_transitions_follow_aaudio() {
    use StreamState::*;
    let expected = [
        (Request::Start, Open, Ok(Started)),
        (Request::Start, Paused, Ok(Started)),
        (Request::Start, Stopped, Ok(Started)),
        (Request::Start, Started, Err(AAUDIO_ERROR_INVALID_STATE)),
        (
            Request::Start,
            Disconnected,
            Err(AAUDIO_ERROR_INVALID_STATE),
        ),
        (Request::Pause, Open, Ok(Paused)),
        (Request::Pause, Started, Ok(Paused)),
        (Request::Pause, Paused, Ok(Paused)),
        (Request::Pause, Stopped, Ok(Paused)),
        (Request::Pause, Disconnected, Ok(Disconnected)),
        (Request::Stop, Open, Ok(Stopped)),
        (Request::Stop, Started, Ok(Stopped)),
        (Request::Stop, Paused, Ok(Stopped)),
        (Request::Stop, Stopped, Ok(Stopped)),
        (Request::Stop, Disconnected, Ok(Disconnected)),
    ];
    for (request, from, to) in expected {
        assert_eq!(request.next_state(from), to, "{request:?} from {from:?}");
    }
}

#[test]
fn stream_control_drives_the_state_machine_through_the_abi() {
    let (stream, _) = detached_stream(Direction::Output);
    unsafe {
        assert_eq!(stream_get_state(stream), StreamState::Open as i32);
        assert_eq!(stream_request_start(stream), AAUDIO_OK);
        assert_eq!(stream_get_state(stream), StreamState::Started as i32);
        assert_eq!(stream_request_start(stream), AAUDIO_ERROR_INVALID_STATE);
        assert_eq!(stream_request_pause(stream), AAUDIO_OK);
        assert_eq!(stream_get_state(stream), StreamState::Paused as i32);
        assert_eq!(stream_request_start(stream), AAUDIO_OK);
        assert_eq!(stream_request_stop(stream), AAUDIO_OK);
        assert_eq!(stream_get_state(stream), StreamState::Stopped as i32);
        assert_eq!(stream_request_stop(stream), AAUDIO_OK, "stop is idempotent");
        assert_eq!(stream_close(stream), AAUDIO_OK);
        assert_eq!(stream_get_state(stream), AAUDIO_ERROR_INVALID_HANDLE);
        assert_eq!(stream_close(stream), AAUDIO_ERROR_INVALID_HANDLE);
    }
}

#[test]
fn input_streams_cannot_pause() {
    let (stream, _) = detached_stream(Direction::Input);
    unsafe {
        assert_eq!(stream_request_start(stream), AAUDIO_OK);
        assert_eq!(stream_request_pause(stream), AAUDIO_ERROR_UNIMPLEMENTED);
        assert_eq!(stream_get_state(stream), StreamState::Started as i32);
        assert_eq!(stream_close(stream), AAUDIO_OK);
    }
}

#[test]
fn disconnected_is_sticky() {
    let (stream, shared) = detached_stream(Direction::Output);
    shared.state.swap(StreamState::Disconnected);
    unsafe {
        assert_eq!(stream_request_start(stream), AAUDIO_ERROR_INVALID_STATE);
        assert_eq!(stream_request_pause(stream), AAUDIO_OK);
        assert_eq!(stream_request_stop(stream), AAUDIO_OK);
        assert_eq!(stream_get_state(stream), StreamState::Disconnected as i32);
        assert_eq!(stream_close(stream), AAUDIO_OK);
    }
}

#[test]
fn getters_report_the_negotiated_stream_truthfully() {
    let (stream, shared) = detached_stream(Direction::Output);
    unsafe {
        assert_eq!(stream_get_format(stream), AAUDIO_FORMAT_PCM_FLOAT);
        assert_eq!(stream_get_sample_rate(stream), 48_000);
        assert_eq!(stream_get_channel_count(stream), 2);
        assert_eq!(stream_get_frames_per_burst(stream), 256);
        assert_eq!(stream_get_buffer_capacity_in_frames(stream), 512);
        assert_eq!(stream_get_buffer_size_in_frames(stream), 512);
        assert_eq!(
            stream_set_buffer_size_in_frames(stream, 100),
            512,
            "the host buffer is fixed, so the actual size is reported"
        );
        assert_eq!(stream_get_xrun_count(stream), 0);
        shared.xrun_count.fetch_add(3, Ordering::Relaxed);
        assert_eq!(stream_get_xrun_count(stream), 3);
        assert_eq!(stream_close(stream), AAUDIO_OK);
    }
}

#[test]
fn read_is_rejected_for_callback_and_output_streams() {
    let (output, _) = detached_stream(Direction::Output);
    let (input, _) = detached_stream(Direction::Input);
    let (read_write, _) = detached_stream_with(Direction::Input, DataPath::ReadWrite);
    let mut buffer = [0i16; 8];
    let buf = buffer.as_mut_ptr().cast::<c_void>();
    unsafe {
        assert_eq!(stream_read(output, buf, 4, 0), AAUDIO_ERROR_UNIMPLEMENTED);
        assert_eq!(stream_read(input, buf, 4, 0), AAUDIO_ERROR_INVALID_STATE);
        assert_eq!(
            stream_read(read_write, buf, 4, 0),
            AAUDIO_ERROR_UNIMPLEMENTED,
            "blocking reads are not provided"
        );
        assert_eq!(stream_close(read_write), AAUDIO_OK);
        assert_eq!(
            stream_read(input, std::ptr::null_mut(), 4, 0),
            AAUDIO_ERROR_NULL
        );
        assert_eq!(
            stream_read(input, buf, -1, 0),
            AAUDIO_ERROR_ILLEGAL_ARGUMENT
        );
        assert_eq!(stream_read(input, buf, 0, 0), 0);
        assert_eq!(stream_close(output), AAUDIO_OK);
        assert_eq!(stream_close(input), AAUDIO_OK);
    }
}

#[test]
fn the_first_read_on_each_stream_is_logged_once() {
    let (read_write, _) = detached_stream_with(Direction::Input, DataPath::ReadWrite);
    let (callback, _) = detached_stream(Direction::Input);
    let mut buffer = [0i16; 8];
    let buf = buffer.as_mut_ptr().cast::<c_void>();
    let log = formatted_log("eclipse::audio=warn", || {
        for _ in 0..3 {
            unsafe {
                assert_eq!(
                    stream_read(read_write, buf, 4, 0),
                    AAUDIO_ERROR_UNIMPLEMENTED
                );
                assert_eq!(stream_read(callback, buf, 4, 0), AAUDIO_ERROR_INVALID_STATE);
            }
        }
    });
    let reads: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("AAudio: read is not supported on this stream"))
        .collect();
    assert_eq!(reads.len(), 2, "{log}");
    for (line, fields) in reads.iter().zip([
        ["WARN eclipse::audio:", "data_path=read_write", "code=-890"],
        ["WARN eclipse::audio:", "data_path=callback", "code=-895"],
    ]) {
        assert!(line.contains("direction=Input"), "{line}");
        for field in fields {
            assert!(line.contains(field), "{field} missing from {line}");
        }
    }
    unsafe {
        assert_eq!(stream_close(read_write), AAUDIO_OK);
        assert_eq!(stream_close(callback), AAUDIO_OK);
    }
}

#[test]
fn null_and_stale_handles_are_errors_not_crashes() {
    let null = std::ptr::null_mut();
    unsafe {
        assert_eq!(
            create_stream_builder(std::ptr::null_mut()),
            AAUDIO_ERROR_NULL
        );
        assert_eq!(builder_delete(null), AAUDIO_ERROR_NULL);
        assert_eq!(stream_get_state(null), AAUDIO_ERROR_NULL);
        assert_eq!(stream_request_start(null), AAUDIO_ERROR_NULL);
        assert_eq!(stream_close(null), AAUDIO_ERROR_NULL);
        let mut out = 0x55 as *mut c_void;
        assert_eq!(builder_open_stream(null, &mut out), AAUDIO_ERROR_NULL);

        let fabricated = ptr_of(0xDEAD_0000_0042);
        assert_eq!(
            stream_get_sample_rate(fabricated),
            AAUDIO_ERROR_INVALID_HANDLE
        );
        assert_eq!(builder_delete(fabricated), AAUDIO_ERROR_INVALID_HANDLE);
        builder_set_direction(fabricated, AAUDIO_DIRECTION_INPUT);
    }
}

#[test]
fn builder_lifecycle_and_open_validation_through_the_abi() {
    let mut builder = std::ptr::null_mut();
    unsafe {
        assert_eq!(create_stream_builder(&mut builder), AAUDIO_OK);
        assert!(!builder.is_null());

        let mut stream = 0x55 as *mut c_void;
        builder_set_format(builder, 9);
        assert_eq!(
            builder_open_stream(builder, &mut stream),
            AAUDIO_ERROR_INVALID_FORMAT
        );
        assert!(stream.is_null(), "a failed open writes NULL");
        builder_set_format(builder, AAUDIO_FORMAT_PCM_I16);
        builder_set_direction(builder, 5);
        assert_eq!(
            builder_open_stream(builder, &mut stream),
            AAUDIO_ERROR_ILLEGAL_ARGUMENT
        );

        assert_eq!(builder_delete(builder), AAUDIO_OK);
        assert_eq!(builder_delete(builder), AAUDIO_ERROR_INVALID_HANDLE);
        assert_eq!(
            builder_open_stream(builder, &mut stream),
            AAUDIO_ERROR_INVALID_HANDLE
        );
    }
}

#[test]
fn wait_for_state_change_reports_changes_and_timeouts() {
    let (stream, _) = detached_stream(Direction::Output);
    let mut next = -1;
    unsafe {
        assert_eq!(
            stream_wait_for_state_change(stream, StreamState::Started as i32, &mut next, 0),
            AAUDIO_OK
        );
        assert_eq!(next, StreamState::Open as i32);
        assert_eq!(
            stream_wait_for_state_change(stream, StreamState::Open as i32, &mut next, 1_000_000),
            AAUDIO_ERROR_TIMEOUT
        );
        assert_eq!(next, StreamState::Open as i32);
    }

    let stream_addr = stream as usize;
    let starter = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        unsafe { stream_request_start(stream_addr as *mut c_void) }
    });
    let began = Instant::now();
    let result = unsafe {
        stream_wait_for_state_change(stream, StreamState::Open as i32, &mut next, 5_000_000_000)
    };
    assert_eq!(starter.join().unwrap(), AAUDIO_OK);
    assert_eq!(result, AAUDIO_OK);
    assert_eq!(next, StreamState::Started as i32);
    assert!(
        began.elapsed() < Duration::from_secs(4),
        "woke on the change"
    );
    unsafe { assert_eq!(stream_close(stream), AAUDIO_OK) };
}

#[test]
fn output_renders_silence_until_started_then_converts_app_samples() {
    let shared = Arc::new(StreamShared::new(4));
    let probe = CallbackProbe::default();
    let mut renderer = renderer::<i16>(&shared, write_i16_ramp, &probe);

    let mut out = [9.0f32; 16];
    renderer.render_output(&mut out, Instant::now());
    assert_eq!(out, [0.0; 16], "an unstarted stream is silent");
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);

    shared.state.swap(StreamState::Started);
    renderer.render_output(&mut out, Instant::now());
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    assert_eq!(*probe.frames.lock().unwrap(), [8]);
    for (i, sample) in out.iter().enumerate() {
        let expected: f32 = cpal::Sample::from_sample((i as i16) * 1000);
        assert_eq!(*sample, expected);
    }
}

#[test]
fn output_larger_than_scratch_is_delivered_in_whole_frame_chunks() {
    let shared = Arc::new(StreamShared::new(4));
    shared.state.swap(StreamState::Started);
    let probe = CallbackProbe::default();
    let mut renderer = renderer::<i16>(&shared, write_i16_ramp, &probe);

    let mut out = [0.0f32; 40];
    renderer.render_output(&mut out, Instant::now());
    assert_eq!(*probe.frames.lock().unwrap(), [8, 8, 4]);
    assert_eq!(
        shared.frames_per_burst.load(Ordering::Relaxed),
        20,
        "the burst reports the device period actually observed"
    );
}

#[test]
fn stop_result_stops_the_stream_and_silences_the_rest() {
    let shared = Arc::new(StreamShared::new(4));
    shared.state.swap(StreamState::Started);
    let probe = CallbackProbe::default();
    probe.result.store(1, Ordering::SeqCst);
    let mut renderer = renderer::<i16>(&shared, write_i16_ramp, &probe);

    let mut out = [7.0f32; 40];
    renderer.render_output(&mut out, Instant::now());
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    assert_eq!(shared.state.load(), StreamState::Stopped);
    assert!(out[16..].iter().all(|&s| s == 0.0));
}

#[test]
fn control_calls_from_the_data_callback_are_rejected() {
    let shared = Arc::new(StreamShared::new(4));
    shared.state.swap(StreamState::Started);
    let probe = CallbackProbe::default();
    let mut renderer = renderer::<i16>(&shared, control_from_callback, &probe);

    let mut out = [0.0f32; 16];
    renderer.render_output(&mut out, Instant::now());
    assert_eq!(
        *probe.collision_results.lock().unwrap(),
        [AAUDIO_ERROR_INVALID_STATE; 4]
    );
    assert_eq!(CALLBACK_STREAM.get(), 0, "the callback marker is cleared");
}

#[test]
fn input_capture_converts_device_samples_for_the_app() {
    let shared = Arc::new(StreamShared::new(4));
    let probe = CallbackProbe::default();
    let mut renderer = renderer::<i16>(&shared, record_i16_input, &probe);

    renderer.capture_input(&[0.5f32, -0.5, 1.0, -1.0], Instant::now());
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0, "not started");

    shared.state.swap(StreamState::Started);
    renderer.capture_input(&[0.5f32, -0.5, 1.0, -1.0], Instant::now());
    let expected: Vec<i16> = [0.5f32, -0.5, 1.0, -1.0]
        .iter()
        .map(|&s| cpal::Sample::from_sample(s))
        .collect();
    assert_eq!(*probe.received.lock().unwrap(), expected);
}

fn error_reporter(shared: &Arc<StreamShared>, probe: &CallbackProbe) -> ErrorReporter {
    ErrorReporter {
        shared: Arc::clone(shared),
        callback: Some(ErrorCallbackTarget {
            func: record_error,
            user_data: probe.user_data(),
        }),
        stream: 0x10,
        streak: HostErrorStreak::default(),
    }
}

fn server_lost() -> cpal::Error {
    cpal::Error::with_message(
        cpal::ErrorKind::BackendError,
        "ALSA function 'snd_pcm_avail_delay' failed with error 'I/O error (5)'",
    )
}

#[test]
fn host_errors_flag_xruns_and_disconnect_once() {
    let shared = Arc::new(StreamShared::new(4));
    shared.state.swap(StreamState::Started);
    let probe = CallbackProbe::default();
    let mut reporter = error_reporter(&shared, &probe);
    let start = Instant::now();

    reporter.report(cpal::ErrorKind::Xrun.into(), start);
    reporter.report(cpal::ErrorKind::Xrun.into(), start);
    reporter.report(cpal::ErrorKind::DeviceChanged.into(), start);
    reporter.report(cpal::ErrorKind::RealtimeDenied.into(), start);
    assert!(shared.host_xrun.load(Ordering::Relaxed));
    assert_eq!(
        shared.xrun_count.load(Ordering::Relaxed),
        0,
        "the next callback counts the flagged xrun"
    );
    assert_eq!(shared.state.load(), StreamState::Started);
    assert!(probe.errors.lock().unwrap().is_empty());

    reporter.report(cpal::ErrorKind::DeviceNotAvailable.into(), start);
    assert_eq!(shared.state.load(), StreamState::Disconnected);
    assert_eq!(*probe.errors.lock().unwrap(), [AAUDIO_ERROR_DISCONNECTED]);

    reporter.report(cpal::ErrorKind::StreamInvalidated.into(), start);
    reporter.report(server_lost(), start);
    reporter.report(server_lost(), start + HOST_ERROR_STREAK_LIMIT);
    assert_eq!(shared.state.load(), StreamState::Disconnected);
    assert_eq!(*probe.errors.lock().unwrap(), [AAUDIO_ERROR_DISCONNECTED]);
}

#[test]
fn a_lost_device_disconnects_the_stream_at_once() {
    for kind in [
        cpal::ErrorKind::DeviceNotAvailable,
        cpal::ErrorKind::StreamInvalidated,
    ] {
        let shared = Arc::new(StreamShared::new(4));
        shared.state.swap(StreamState::Started);
        let probe = CallbackProbe::default();
        let mut reporter = error_reporter(&shared, &probe);
        reporter.report(kind.into(), Instant::now());
        assert_eq!(shared.state.load(), StreamState::Disconnected, "{kind:?}");
        assert_eq!(
            *probe.errors.lock().unwrap(),
            [AAUDIO_ERROR_DISCONNECTED],
            "{kind:?}"
        );
    }
}

#[test]
fn host_errors_without_host_callbacks_disconnect_once_after_the_streak_limit() {
    for kind in [
        cpal::ErrorKind::BackendError,
        cpal::ErrorKind::DeviceBusy,
        cpal::ErrorKind::HostUnavailable,
        cpal::ErrorKind::InvalidInput,
        cpal::ErrorKind::PermissionDenied,
        cpal::ErrorKind::ResourceExhausted,
        cpal::ErrorKind::UnsupportedConfig,
        cpal::ErrorKind::UnsupportedOperation,
        cpal::ErrorKind::Other,
    ] {
        let shared = Arc::new(StreamShared::new(4));
        shared.state.swap(StreamState::Started);
        let probe = CallbackProbe::default();
        let mut reporter = error_reporter(&shared, &probe);
        let start = Instant::now();

        reporter.report(kind.into(), start);
        reporter.report(
            kind.into(),
            start + HOST_ERROR_STREAK_LIMIT - Duration::from_millis(1),
        );
        assert_eq!(shared.state.load(), StreamState::Started, "{kind:?}");
        assert!(probe.errors.lock().unwrap().is_empty(), "{kind:?}");

        reporter.report(kind.into(), start + HOST_ERROR_STREAK_LIMIT);
        reporter.report(kind.into(), start + 2 * HOST_ERROR_STREAK_LIMIT);
        assert_eq!(shared.state.load(), StreamState::Disconnected, "{kind:?}");
        assert_eq!(
            *probe.errors.lock().unwrap(),
            [AAUDIO_ERROR_DISCONNECTED],
            "{kind:?}"
        );
    }
}

#[test]
fn a_host_error_followed_by_a_host_callback_keeps_the_stream_started() {
    let shared = Arc::new(StreamShared::new(4));
    shared.state.swap(StreamState::Started);
    let probe = CallbackProbe::default();
    let mut renderer = renderer::<i16>(&shared, write_i16_ramp, &probe);
    let mut reporter = error_reporter(&shared, &probe);
    let start = Instant::now();

    reporter.report(server_lost(), start);
    renderer.render_output(&mut [0.0f32; 16], Instant::now());
    reporter.report(server_lost(), start + HOST_ERROR_STREAK_LIMIT);
    assert_eq!(shared.state.load(), StreamState::Started);
    assert!(probe.errors.lock().unwrap().is_empty());
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);

    reporter.report(server_lost(), start + 2 * HOST_ERROR_STREAK_LIMIT);
    assert_eq!(
        shared.state.load(),
        StreamState::Disconnected,
        "the error after the callback starts a new streak"
    );
    assert_eq!(*probe.errors.lock().unwrap(), [AAUDIO_ERROR_DISCONNECTED]);
}

#[test]
fn a_route_change_disconnects_a_running_stream_once() {
    static ROUTE: AtomicU64 = AtomicU64::new(0);
    let shared = Arc::new(StreamShared::new(4));
    shared.state.swap(StreamState::Started);
    let probe = CallbackProbe::default();
    let mut renderer = routed_renderer::<i16>(&shared, write_i16_ramp, &probe, Route::on(&ROUTE));

    renderer.render_output(&mut [0.0f32; 16], Instant::now());
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    assert!(probe.errors.lock().unwrap().is_empty());

    ROUTE.fetch_add(1, Ordering::AcqRel);
    let mut out = [1.0f32; 16];
    renderer.render_output(&mut out, Instant::now());
    renderer.render_output(&mut out, Instant::now());
    assert_eq!(shared.state.load(), StreamState::Disconnected);
    assert_eq!(*probe.errors.lock().unwrap(), [AAUDIO_ERROR_DISCONNECTED]);
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    assert_eq!(out, [0.0f32; 16]);
}

const LOW_LATENCY_BURST: Duration = Duration::from_nanos(10_666_667);

const SERVER_QUANTUM: Duration = Duration::from_nanos(21_333_333);

const RUNNING: Option<Run> = Some(Run(1));

const SECOND: Duration = Duration::from_secs(1);

const LOW_LATENCY_FRAMES: usize = 512;

const CAPACITY_FRAMES: u32 = 1024;

const PIPEWIRE_PULSE_FRAMES: usize = 341;

struct CallbackClock {
    detector: LateCallbackDetector,
    frames: usize,
    at: Instant,
    host_xrun: AtomicBool,
}

impl CallbackClock {
    fn new() -> Self {
        Self::with_frames(LOW_LATENCY_FRAMES)
    }

    fn with_frames(frames: usize) -> Self {
        Self {
            detector: LateCallbackDetector::new(CAPACITY_FRAMES, 48_000),
            frames,
            at: Instant::now(),
            host_xrun: AtomicBool::new(false),
        }
    }

    fn tick(&mut self, gap: Duration, run: Option<Run>) -> bool {
        self.at += gap;
        self.detector
            .underrun(self.at, self.frames, run, &self.host_xrun)
    }

    fn run(&mut self, gaps: impl IntoIterator<Item = Duration>, run: Option<Run>) -> usize {
        gaps.into_iter().filter(|gap| self.tick(*gap, run)).count()
    }
}

fn steady(gap: Duration, length: Duration) -> impl Iterator<Item = Duration> + Clone {
    std::iter::repeat_n(gap, length.as_nanos().div_ceil(gap.as_nanos()) as usize)
}

fn past_the_start() -> impl Iterator<Item = Duration> + Clone {
    steady(LOW_LATENCY_BURST, START_GRACE + LOW_LATENCY_BURST)
}

#[test]
fn steady_low_latency_callbacks_are_never_late() {
    let mut clock = CallbackClock::new();
    assert_eq!(
        clock.run(steady(LOW_LATENCY_BURST, 10 * SECOND), RUNNING),
        0
    );
}

#[test]
fn back_to_back_pairs_every_server_quantum_are_never_late() {
    let pair = [
        Duration::from_micros(50),
        SERVER_QUANTUM - Duration::from_micros(50),
    ];
    let mut clock = CallbackClock::new();
    assert_eq!(clock.run(pair.into_iter().cycle().take(1_000), RUNNING), 0);
}

#[test]
fn one_60_ms_gap_is_one_underrun() {
    let gaps = past_the_start()
        .chain([Duration::from_millis(60)])
        .chain(steady(LOW_LATENCY_BURST, SECOND));
    assert_eq!(CallbackClock::new().run(gaps, RUNNING), 1);
}

#[test]
fn a_callback_is_late_after_twice_the_buffer_of_a_low_latency_stream() {
    let mut clock = CallbackClock::new();
    assert_eq!(clock.run(past_the_start(), RUNNING), 0);
    assert!(!clock.tick(Duration::from_micros(42_600), RUNNING));
    assert!(
        clock.tick(Duration::from_micros(42_700), RUNNING),
        "1024 frames last 21.3 ms at 48 kHz"
    );
}

#[test]
fn the_first_second_of_a_run_is_not_judged() {
    let gaps = [
        LOW_LATENCY_BURST,
        Duration::from_millis(60),
        LOW_LATENCY_BURST,
    ];
    assert_eq!(CallbackClock::new().run(gaps, RUNNING), 0);
}

#[test]
fn the_audio_a_burst_delivered_covers_the_wait_after_it() {
    let spacing = Duration::from_micros(50);
    let rest_of_burst = [spacing; 5];
    let cycles = (0..100).flat_map(|index| {
        let wait = if index % 7 == 0 { 46_200 } else { 42_400 };
        rest_of_burst
            .into_iter()
            .chain([Duration::from_micros(wait)])
    });
    let mut clock = CallbackClock::with_frames(PIPEWIRE_PULSE_FRAMES);
    assert_eq!(clock.run(cycles, RUNNING), 0, "2048-frame quantum");

    let callback = Duration::from_secs(PIPEWIRE_PULSE_FRAMES as u64) / 48_000;
    let buffer = Duration::from_secs(u64::from(CAPACITY_FRAMES)) / 48_000;
    let still_ahead = 6 * callback - 5 * spacing;
    assert_eq!(clock.run(rest_of_burst, RUNNING), 0);
    assert!(!clock.tick(still_ahead + buffer, RUNNING));
    assert_eq!(clock.run(rest_of_burst, RUNNING), 0);
    assert!(
        clock.tick(still_ahead + buffer + Duration::from_micros(1), RUNNING),
        "a buffer beyond the audio the server still had"
    );
}

#[test]
fn an_isolated_stall_does_not_raise_the_limit() {
    let stall = [Duration::from_millis(60)];
    let gaps = past_the_start()
        .chain(stall)
        .chain(steady(LOW_LATENCY_BURST, SECOND * 3 / 2))
        .chain(stall)
        .chain(steady(LOW_LATENCY_BURST, SECOND));
    assert_eq!(CallbackClock::new().run(gaps, RUNNING), 2);
}

#[test]
fn a_stall_in_every_second_is_counted_every_time() {
    let cycle = steady(LOW_LATENCY_BURST, SECOND / 2).chain([Duration::from_millis(60)]);
    let gaps = past_the_start().chain(std::iter::repeat_n(cycle, 20).flatten());
    assert_eq!(CallbackClock::new().run(gaps, RUNNING), 20);
}

#[test]
fn a_device_clock_running_fast_never_raises_the_limit() {
    let fast = LOW_LATENCY_BURST - LOW_LATENCY_BURST / 1000;
    let gaps = steady(fast, 600 * SECOND).chain([Duration::from_millis(60)]);
    assert_eq!(CallbackClock::new().run(gaps, RUNNING), 1);
}

#[test]
fn a_gap_across_a_stop_and_start_is_not_an_underrun() {
    let mut paused_host = CallbackClock::new();
    assert_eq!(paused_host.run(past_the_start(), Some(Run(1))), 0);
    assert!(!paused_host.tick(SECOND, Some(Run(2))));

    let mut running_host = CallbackClock::new();
    assert_eq!(running_host.run(past_the_start(), Some(Run(1))), 0);
    assert!(!running_host.tick(Duration::from_millis(200), None));
    assert!(!running_host.tick(SECOND, Some(Run(1))));
}

#[test]
fn a_cpal_xrun_and_the_late_callback_after_it_count_once() {
    let mut clock = CallbackClock::new();
    assert_eq!(clock.run(past_the_start(), RUNNING), 0);
    clock.host_xrun.store(true, Ordering::Relaxed);
    let after = [Duration::from_millis(80), LOW_LATENCY_BURST];
    assert_eq!(clock.run(after, RUNNING), 1);
    assert!(
        !clock.host_xrun.load(Ordering::Relaxed),
        "the report is consumed"
    );
}

#[test]
fn a_cpal_xrun_counts_in_the_first_second() {
    let mut clock = CallbackClock::new();
    assert!(!clock.tick(LOW_LATENCY_BURST, RUNNING));
    clock.host_xrun.store(true, Ordering::Relaxed);
    assert!(clock.tick(LOW_LATENCY_BURST, RUNNING));
}

#[test]
fn a_cpal_xrun_while_stopped_is_not_counted_after_the_restart() {
    let mut clock = CallbackClock::new();
    clock.host_xrun.store(true, Ordering::Relaxed);
    assert!(!clock.tick(LOW_LATENCY_BURST, None));
    assert!(!clock.tick(LOW_LATENCY_BURST, Some(Run(2))));
}

#[test]
fn late_callback_warnings_wait_ten_seconds_and_need_a_new_count() {
    let start = Instant::now();
    let mut log = LateCallbackLog::default();
    assert!(!log.due(0, start), "nothing to report");
    assert!(log.due(1, start));
    assert!(!log.due(2, start + SECOND));
    assert!(log.due(3, start + 12 * SECOND));
    assert!(
        !log.due(3, start + 30 * SECOND),
        "the count has not changed"
    );
}

fn render_at(renderer: &mut Renderer<i16>, start: Instant, millis: impl IntoIterator<Item = u64>) {
    for millis in millis {
        renderer.render_output(&mut [0.0f32; 16], start + Duration::from_millis(millis));
    }
}

#[test]
fn late_callbacks_raise_the_xrun_count_until_a_restart() {
    let (stream, shared) = detached_stream(Direction::Output);
    let probe = CallbackProbe::default();
    let mut renderer = renderer::<i16>(&shared, write_i16_ramp, &probe);
    let mut reporter = error_reporter(&shared, &probe);
    let start = Instant::now();
    unsafe {
        assert_eq!(stream_request_start(stream), AAUDIO_OK);
        render_at(&mut renderer, start, (0..=1_000).step_by(5));
        assert_eq!(stream_get_xrun_count(stream), 0);
        render_at(&mut renderer, start, [1_030]);
        assert_eq!(
            stream_get_xrun_count(stream),
            1,
            "a 30 ms gap on a 512-frame buffer"
        );
        reporter.report(cpal::ErrorKind::Xrun.into(), start);
        render_at(&mut renderer, start, [1_100]);
        assert_eq!(
            stream_get_xrun_count(stream),
            2,
            "the xrun and its gap count once"
        );
        assert_eq!(stream_request_stop(stream), AAUDIO_OK);
        assert_eq!(stream_request_start(stream), AAUDIO_OK);
        render_at(&mut renderer, start, [3_000, 3_005]);
        assert_eq!(
            stream_get_xrun_count(stream),
            2,
            "the restart is not an underrun"
        );
        assert_eq!(stream_close(stream), AAUDIO_OK);
    }
}

#[test]
fn late_input_callbacks_raise_the_xrun_count() {
    let shared = Arc::new(StreamShared::new(256));
    shared.state.swap(StreamState::Started);
    let probe = CallbackProbe::default();
    let mut renderer = renderer::<i16>(&shared, record_i16_input, &probe);
    let start = Instant::now();
    for millis in (0..=1_000).step_by(5).chain([1_030]) {
        renderer.capture_input(&[0.0f32; 4], start + Duration::from_millis(millis));
    }
    assert_eq!(shared.xrun_count.load(Ordering::Relaxed), 1);
}

#[test]
fn late_callbacks_warn_at_most_every_ten_seconds_and_the_close_line_has_the_total() {
    let (stream, shared) = detached_stream(Direction::Output);
    let probe = CallbackProbe::default();
    let mut renderer = renderer::<i16>(&shared, write_i16_ramp, &probe);
    let start = Instant::now();
    let log = formatted_log("eclipse::audio=info", || unsafe {
        assert_eq!(stream_request_start(stream), AAUDIO_OK);
        render_at(&mut renderer, start, (0..=1_000).step_by(10));
        render_at(
            &mut renderer,
            start,
            [1_060, 1_070, 2_070, 2_080, 13_080, 13_090],
        );
        assert_eq!(stream_close(stream), AAUDIO_OK);
    });
    let warnings: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("AAudio: late callbacks; audio likely dropped out"))
        .collect();
    assert_eq!(warnings.len(), 2, "{log}");
    for (line, xruns) in warnings.iter().zip(["xruns=1", "xruns=3"]) {
        for field in ["WARN eclipse::audio:", "direction=Output", xruns] {
            assert!(line.contains(field), "{field} missing from {line}");
        }
    }
    let closed: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("AAudio: stream closed"))
        .collect();
    assert_eq!(closed.len(), 1, "{log}");
    for field in ["INFO eclipse::audio:", "direction=Output", "xruns=3"] {
        assert!(
            closed[0].contains(field),
            "{field} missing from {}",
            closed[0]
        );
    }
}

type DlopenFn = unsafe extern "C" fn(*const c_char, c_int) -> *mut c_void;
type DlsymFn = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void;

struct FmodView {
    create: unsafe extern "C" fn(*mut *mut c_void) -> i32,
    delete: unsafe extern "C" fn(*mut c_void) -> i32,
    open: unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> i32,
    set_i32: [unsafe extern "C" fn(*mut c_void, i32); 4],
    set_data: unsafe extern "C" fn(*mut c_void, Option<DataCallback>, *mut c_void),
    set_error: unsafe extern "C" fn(*mut c_void, Option<ErrorCallback>, *mut c_void),
    get: [unsafe extern "C" fn(*mut c_void) -> i32; 6],
    set_size: unsafe extern "C" fn(*mut c_void, i32) -> i32,
    start: unsafe extern "C" fn(*mut c_void) -> i32,
    stop: unsafe extern "C" fn(*mut c_void) -> i32,
    state: unsafe extern "C" fn(*mut c_void) -> i32,
    wait: unsafe extern "C" fn(*mut c_void, i32, *mut i32, i64) -> i32,
    close: unsafe extern "C" fn(*mut c_void) -> i32,
}

struct LibraryView {
    library: *mut c_void,
    dlsym: DlsymFn,
}

impl LibraryView {
    fn open() -> Self {
        let provider = EclipseNativeProvider::with_bionic_natives();
        let native = |name: &str| provider.resolve(name).unwrap().addr as usize as *const ();
        let (dlopen, dlsym) = unsafe {
            (
                std::mem::transmute::<*const (), DlopenFn>(native("dlopen")),
                std::mem::transmute::<*const (), DlsymFn>(native("dlsym")),
            )
        };
        let library = unsafe { dlopen(c"libaaudio.so".as_ptr(), 1) };
        assert!(!library.is_null());
        Self { library, dlsym }
    }

    fn get<F: Copy>(&self, name: &CStr) -> F {
        let found = unsafe { (self.dlsym)(self.library, name.as_ptr()) };
        assert!(!found.is_null(), "{name:?}");
        assert_eq!(size_of::<F>(), size_of::<*mut c_void>());
        unsafe { std::mem::transmute_copy::<*mut c_void, F>(&found) }
    }
}

fn fmod_view() -> FmodView {
    let lib = LibraryView::open();
    FmodView {
        create: lib.get(c"AAudio_createStreamBuilder"),
        delete: lib.get(c"AAudioStreamBuilder_delete"),
        open: lib.get(c"AAudioStreamBuilder_openStream"),
        set_i32: [
            lib.get(c"AAudioStreamBuilder_setPerformanceMode"),
            lib.get(c"AAudioStreamBuilder_setUsage"),
            lib.get(c"AAudioStreamBuilder_setDirection"),
            lib.get(c"AAudioStreamBuilder_setBufferCapacityInFrames"),
        ],
        set_data: lib.get(c"AAudioStreamBuilder_setDataCallback"),
        set_error: lib.get(c"AAudioStreamBuilder_setErrorCallback"),
        get: [
            lib.get(c"AAudioStream_getBufferSizeInFrames"),
            lib.get(c"AAudioStream_getSampleRate"),
            lib.get(c"AAudioStream_getFramesPerBurst"),
            lib.get(c"AAudioStream_getBufferCapacityInFrames"),
            lib.get(c"AAudioStream_getFormat"),
            lib.get(c"AAudioStream_getChannelCount"),
        ],
        set_size: lib.get(c"AAudioStream_setBufferSizeInFrames"),
        start: lib.get(c"AAudioStream_requestStart"),
        stop: lib.get(c"AAudioStream_requestStop"),
        state: lib.get(c"AAudioStream_getState"),
        wait: lib.get(c"AAudioStream_waitForStateChange"),
        close: lib.get(c"AAudioStream_close"),
    }
}

fn default_device_config(direction: Direction) -> cpal::SupportedStreamConfig {
    let host = cpal::default_host();
    match direction {
        Direction::Output => host
            .default_output_device()
            .expect("ALSA names a default device")
            .default_output_config(),
        Direction::Input => host
            .default_input_device()
            .expect("ALSA names a default device")
            .default_input_config(),
    }
    .expect("the private sound server's default device has a configuration")
}

fn fmod_driver_probe(api: &FmodView, direction: i32) -> Result<(i32, i32), i32> {
    let mut builder = std::ptr::null_mut();
    let mut stream = std::ptr::null_mut();
    unsafe {
        assert_eq!((api.create)(&mut builder), AAUDIO_OK);
        (api.set_i32[0])(builder, AAUDIO_PERFORMANCE_MODE_LOW_LATENCY);
        (api.set_i32[1])(builder, 14);
        (api.set_i32[2])(builder, direction);
        let opened = (api.open)(builder, &mut stream);
        let reported = if opened == AAUDIO_OK {
            assert!(
                streams()
                    .with(handle_of(stream), |entry| entry.host.is_none())
                    .unwrap(),
                "a probe without a data callback does not build a host stream"
            );
            Ok(((api.get[1])(stream), (api.get[5])(stream)))
        } else {
            Err(opened)
        };
        assert_eq!((api.delete)(builder), AAUDIO_OK);
        if opened == AAUDIO_OK {
            assert_eq!((api.close)(stream), AAUDIO_OK);
        }
        reported
    }
}

fn offers_48_khz_stereo(direction: Direction) -> bool {
    let host = cpal::default_host();
    let ranges = match direction {
        Direction::Output => host
            .default_output_device()
            .and_then(|device| device.supported_output_configs().ok())
            .map(Iterator::collect::<Vec<_>>),
        Direction::Input => host
            .default_input_device()
            .and_then(|device| device.supported_input_configs().ok())
            .map(Iterator::collect::<Vec<_>>),
    };
    ranges
        .unwrap_or_default()
        .iter()
        .any(|range| range.channels() == APP_CHANNELS && range.contains_rate(APP_SAMPLE_RATE))
}

fn probe_both_directions(_: &NullDevices) {
    let api = fmod_view();
    for (direction, raw) in [
        (Direction::Output, AAUDIO_DIRECTION_OUTPUT),
        (Direction::Input, AAUDIO_DIRECTION_INPUT),
    ] {
        let config = default_device_config(direction);
        let expected = if offers_48_khz_stereo(direction) {
            (APP_SAMPLE_RATE as i32, i32::from(APP_CHANNELS))
        } else {
            (config.sample_rate() as i32, i32::from(config.channels()))
        };
        assert_eq!(
            fmod_driver_probe(&api, raw),
            Ok(expected),
            "{direction:?} probe reports 48 kHz stereo when the device offers it"
        );
    }
}

#[test]
fn fmod_driver_probe_opens_callback_less_streams_in_both_directions() {
    on_private_server(
        "loader::aaudio::tests::fmod_driver_probe_opens_callback_less_streams_in_both_directions",
        probe_both_directions,
    );
}

fn open_a_stream_on_each_data_path(_: &NullDevices) {
    let api = fmod_view();
    let probe = CallbackProbe::default();
    let callbacks: [Option<DataCallback>; 2] = [None, Some(write_silence)];
    let log = formatted_log("eclipse::audio=info", || unsafe {
        let mut builder = std::ptr::null_mut();
        assert_eq!((api.create)(&mut builder), AAUDIO_OK);
        (api.set_i32[2])(builder, AAUDIO_DIRECTION_OUTPUT);
        for callback in callbacks {
            (api.set_data)(builder, callback, probe.user_data() as *mut c_void);
            let mut stream = std::ptr::null_mut();
            assert_eq!((api.open)(builder, &mut stream), AAUDIO_OK);
            assert_eq!((api.close)(stream), AAUDIO_OK);
        }
        assert_eq!((api.delete)(builder), AAUDIO_OK);
    });
    let opened: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("AAudio: stream opened"))
        .collect();
    assert_eq!(opened.len(), 2, "{log}");
    assert!(opened[0].contains("data_path=read_write"), "{}", opened[0]);
    assert!(opened[1].contains("data_path=callback"), "{}", opened[1]);
}

#[test]
fn the_stream_opened_line_names_the_data_path() {
    on_private_server(
        "loader::aaudio::tests::the_stream_opened_line_names_the_data_path",
        open_a_stream_on_each_data_path,
    );
}

fn play_the_fmod_output_sequence(_: &NullDevices) {
    let api = fmod_view();
    let (probed_rate, probed_channels) = fmod_driver_probe(&api, AAUDIO_DIRECTION_OUTPUT)
        .expect("the private sound server's default output opens");
    let probe = CallbackProbe::default();
    let user = probe.user_data() as *mut c_void;
    let mut builder = std::ptr::null_mut();
    let mut stream = std::ptr::null_mut();
    unsafe {
        assert_eq!((api.create)(&mut builder), AAUDIO_OK);
        (api.set_i32[0])(builder, AAUDIO_PERFORMANCE_MODE_LOW_LATENCY);
        (api.set_i32[1])(builder, 14);
        (api.set_i32[2])(builder, AAUDIO_DIRECTION_OUTPUT);
        (api.set_data)(builder, Some(write_silence), user);
        (api.set_error)(builder, Some(record_error), user);

        assert_eq!((api.open)(builder, &mut stream), AAUDIO_OK);
        let buffer_size = (api.get[0])(stream);
        let sample_rate = (api.get[1])(stream);
        let burst = (api.get[2])(stream);
        let capacity = (api.get[3])(stream);
        assert_eq!(
            sample_rate, probed_rate,
            "the probe and the real open agree"
        );
        assert!(burst > 0 && capacity >= burst && buffer_size <= capacity);
        assert_eq!((api.close)(stream), AAUDIO_OK);

        (api.set_i32[3])(builder, 2 * buffer_size);
        assert_eq!((api.open)(builder, &mut stream), AAUDIO_OK);
        let format = (api.get[4])(stream);
        let channels = (api.get[5])(stream);
        assert_eq!(channels, probed_channels);
        let sample_bytes = match format {
            AAUDIO_FORMAT_PCM_I16 => 2,
            AAUDIO_FORMAT_PCM_FLOAT => 4,
            other => panic!("unexpected format {other}"),
        };
        probe
            .bytes_per_frame
            .store(sample_bytes * channels as usize, Ordering::SeqCst);
        assert!((api.set_size)(stream, buffer_size) > 0);

        assert_eq!((api.start)(stream), AAUDIO_OK);
        assert_eq!((api.state)(stream), StreamState::Started as i32);
        let deadline = Instant::now() + Duration::from_secs(3);
        while probe.calls.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            probe.calls.load(Ordering::SeqCst) > 0,
            "the host device pulled audio through the data callback"
        );

        assert_eq!((api.stop)(stream), AAUDIO_OK);
        let mut state = (api.state)(stream);
        while state != StreamState::Stopped as i32 {
            let mut next = state;
            assert_eq!(
                (api.wait)(stream, state, &mut next, 1_000_000_000),
                AAUDIO_OK
            );
            state = next;
        }
        std::thread::sleep(Duration::from_millis(20));
        let settled = probe.calls.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            probe.calls.load(Ordering::SeqCst),
            settled,
            "a stopped stream no longer calls the app"
        );
        assert_eq!((api.close)(stream), AAUDIO_OK);
        assert_eq!((api.delete)(builder), AAUDIO_OK);
    }
    assert!(probe.errors.lock().unwrap().is_empty());
}

#[test]
fn fmod_output_sequence_plays_through_the_host_device() {
    on_private_server(
        "loader::aaudio::tests::fmod_output_sequence_plays_through_the_host_device",
        play_the_fmod_output_sequence,
    );
}

const ROUTED_CHILD: &str = "ECLIPSE_TEST_ROUTED_AAUDIO_CHILD";

const ROUTED_TEST: &str =
    "loader::aaudio::tests::aaudio_streams_reach_the_devices_chosen_in_the_settings";

const ROUTED_LIMIT: Duration = Duration::from_secs(30);

unsafe extern "C" fn count_input(
    _stream: *mut c_void,
    user_data: *mut c_void,
    _audio: *mut c_void,
    _frames: i32,
) -> i32 {
    let probe = unsafe { &*(user_data as *const CallbackProbe) };
    probe.calls.fetch_add(1, Ordering::SeqCst);
    AAUDIO_CALLBACK_RESULT_CONTINUE
}

fn started_stream(
    api: &FmodView,
    direction: i32,
    callback: DataCallback,
    probe: &CallbackProbe,
) -> *mut c_void {
    let mut builder = std::ptr::null_mut();
    let mut stream = std::ptr::null_mut();
    unsafe {
        assert_eq!((api.create)(&mut builder), AAUDIO_OK);
        (api.set_i32[2])(builder, direction);
        (api.set_data)(builder, Some(callback), probe.user_data() as *mut c_void);
        (api.set_error)(
            builder,
            Some(record_error),
            probe.user_data() as *mut c_void,
        );
        assert_eq!((api.open)(builder, &mut stream), AAUDIO_OK);
        assert_eq!((api.delete)(builder), AAUDIO_OK);
        assert_eq!(
            ((api.get[1])(stream), (api.get[5])(stream)),
            (APP_SAMPLE_RATE as i32, i32::from(APP_CHANNELS)),
            "streams on the PulseAudio plug-in open at 48 kHz stereo"
        );
        let sample_bytes = match (api.get[4])(stream) {
            AAUDIO_FORMAT_PCM_I16 => 2,
            AAUDIO_FORMAT_PCM_FLOAT => 4,
            other => panic!("unexpected format {other}"),
        };
        let channels = usize::try_from((api.get[5])(stream)).expect("a channel count");
        probe
            .bytes_per_frame
            .store(sample_bytes * channels, Ordering::SeqCst);
        assert_eq!((api.start)(stream), AAUDIO_OK);
    }
    stream
}

fn play_silence_and_record_until(stop: &Path) {
    let pulse = cpal::DeviceId::new(cpal::HostId::Alsa, crate::audio::PULSE_PCM);
    for direction in Direction::ALL {
        let device = crate::audio::host_device(direction).expect("a host device");
        assert_eq!(
            device.id().ok().as_ref(),
            Some(&pulse),
            "{direction:?} streams must open the PulseAudio plug-in, which plays to the chosen device"
        );
    }
    let api = fmod_view();
    let output = CallbackProbe::default();
    let input = CallbackProbe::default();
    let streams = [
        started_stream(&api, AAUDIO_DIRECTION_OUTPUT, write_silence, &output),
        started_stream(&api, AAUDIO_DIRECTION_INPUT, count_input, &input),
    ];
    let deadline = Instant::now() + ROUTED_LIMIT;
    loop {
        let stopped = stop.exists();
        let calls = [&output, &input].map(|probe| probe.calls.load(Ordering::SeqCst));
        if stopped && calls.iter().all(|&calls| calls > 0) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "after {ROUTED_LIMIT:?}: stopped by the parent test: {stopped}, \
             output and input callbacks: {calls:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    for stream in streams {
        unsafe {
            assert_eq!((api.stop)(stream), AAUDIO_OK);
            assert_eq!((api.close)(stream), AAUDIO_OK);
        }
    }
}

fn routing_to(devices: &NullDevices) -> Routing {
    Routing::plan(&eclipse_config::Config {
        audio_output_device: AudioDevice::Named(devices.sink.clone()),
        audio_input_device: AudioDevice::Named(devices.source.clone()),
        ..eclipse_config::Config::default()
    })
}

struct StopFile(PathBuf);

impl StopFile {
    fn new(purpose: &str) -> Self {
        let stop = std::env::temp_dir().join(format!("eclipse-{purpose}-{}", std::process::id()));
        std::fs::remove_file(&stop).ok();
        Self(stop)
    }

    fn stop(&self) {
        std::fs::write(&self.0, "").expect("stop the player");
    }
}

impl Drop for StopFile {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
    }
}

#[test]
fn a_failed_sound_test_leaves_no_stop_file() {
    let mut written = None;
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let stop = StopFile::new("failed-test");
        stop.stop();
        written = Some(stop.0.clone());
        panic!("the sound test fails after stopping its player");
    }));
    assert!(failed.is_err());
    let written = written.expect("the test wrote its stop file");
    assert!(!written.exists(), "{} was left behind", written.display());
}

fn route_the_streams_to_the_chosen_devices(_: &NullDevices) {
    let chosen = NullDevices::create("chosen");
    let routing = routing_to(&chosen);
    let stop = StopFile::new("aaudio-routed");
    let mut player = run_child_test(ROUTED_TEST, ROUTED_CHILD, stop.0.as_os_str(), |command| {
        routing.apply(command);
    });
    wait_for_streams_on(&mut player, &chosen, "while the player plays and records");
    stop.stop();
    assert_passed(&player.finish());
}

#[test]
fn aaudio_streams_reach_the_devices_chosen_in_the_settings() {
    if let Some(stop) = std::env::var_os(ROUTED_CHILD) {
        play_silence_and_record_until(Path::new(&stop));
        return;
    }
    on_private_server(ROUTED_TEST, route_the_streams_to_the_chosen_devices);
}

fn stop_the_player_before_it_starts(defaults: &NullDevices) {
    let routing = routing_to(defaults);
    let stop = StopFile::new("aaudio-stopped");
    stop.stop();
    let player = run_child_test(ROUTED_TEST, ROUTED_CHILD, stop.0.as_os_str(), |command| {
        routing.apply(command);
    });
    assert_passed(&player.finish());
}

#[test]
fn a_player_stopped_at_once_keeps_its_streams_until_both_call_back() {
    on_private_server(
        "loader::aaudio::tests::a_player_stopped_at_once_keeps_its_streams_until_both_call_back",
        stop_the_player_before_it_starts,
    );
}

fn wait_for_a_player_that_cannot_route(defaults: &NullDevices) {
    let stop = StopFile::new("aaudio-unrouted");
    let mut player = run_child_test(ROUTED_TEST, ROUTED_CHILD, stop.0.as_os_str(), |_| {});
    let started = Instant::now();
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        wait_for_streams_on(&mut player, defaults, "while the unrouted player starts");
    }))
    .expect_err("a player without chosen devices cannot open the PulseAudio plug-in");
    let waited = started.elapsed();
    let said = failure.downcast_ref::<String>().map_or("", String::as_str);
    assert!(said.contains("must open the PulseAudio plug-in"), "{said}");
    assert!(
        waited < MOVE_LIMIT,
        "the wait outlived the player: {waited:?}"
    );
}

#[test]
fn a_player_that_fails_ends_the_wait_with_its_own_error() {
    on_private_server(
        "loader::aaudio::tests::a_player_that_fails_ends_the_wait_with_its_own_error",
        wait_for_a_player_that_cannot_route,
    );
}

const FOLLOW_PLAYER: &str = "ECLIPSE_TEST_FOLLOW_PLAYER";

const FOLLOW_TEST: &str =
    "loader::aaudio::tests::streams_follow_a_chosen_device_that_returns_or_stops_being_the_default";

const FOLLOW_LIMIT: Duration = Duration::from_secs(120);

const MOVE_LIMIT: Duration = Duration::from_secs(10);

const SETTLE: Duration = Duration::from_millis(500);

struct ChildTest(Option<Child>);

impl ChildTest {
    fn id(&self) -> u32 {
        self.0.as_ref().expect("the child test runs").id()
    }

    fn running(&mut self) -> bool {
        self.0
            .as_mut()
            .expect("the child test runs")
            .try_wait()
            .expect("check on the child test")
            .is_none()
    }

    fn finish(mut self) -> Output {
        self.0
            .take()
            .expect("the child test runs")
            .wait_with_output()
            .expect("wait for the child test")
    }

    fn fail(&mut self, problem: &str) -> ! {
        let mut child = self.0.take().expect("the child test runs");
        child.kill().ok();
        let output = child.wait_with_output().expect("wait for the child test");
        panic!("{problem}; the child test ended with {}", outcome(&output));
    }
}

impl Drop for ChildTest {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            child.kill().ok();
            child.wait().ok();
        }
    }
}

fn run_child_test(
    test: &str,
    variable: &str,
    value: &OsStr,
    configure: impl FnOnce(&mut Command),
) -> ChildTest {
    let mut command = test_command(test);
    command
        .env(variable, value)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure(&mut command);
    ChildTest(Some(command.spawn().expect("start the child test")))
}

fn play_and_record_reopening_until(stop: &Path) {
    crate::audio::hotplug::follow_chosen_devices(crate::loader::reopen_audio_streams);
    let api = fmod_view();
    let output = CallbackProbe::default();
    let input = CallbackProbe::default();
    let open = |direction| match direction {
        AAUDIO_DIRECTION_OUTPUT => started_stream(&api, direction, write_silence, &output),
        _ => started_stream(&api, direction, count_input, &input),
    };
    let mut streams = [
        (
            AAUDIO_DIRECTION_OUTPUT,
            &output,
            open(AAUDIO_DIRECTION_OUTPUT),
        ),
        (AAUDIO_DIRECTION_INPUT, &input, open(AAUDIO_DIRECTION_INPUT)),
    ];
    let deadline = Instant::now() + FOLLOW_LIMIT;
    while !stop.exists() {
        assert!(
            Instant::now() < deadline,
            "the controller never stopped the player"
        );
        for (direction, probe, stream) in &mut streams {
            let errors = std::mem::take(&mut *probe.errors.lock().unwrap());
            if errors.is_empty() {
                continue;
            }
            assert_eq!(errors, [AAUDIO_ERROR_DISCONNECTED]);
            unsafe { assert_eq!((api.close)(*stream), AAUDIO_OK) };
            *stream = open(*direction);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    for (_, _, stream) in streams {
        unsafe {
            assert_eq!((api.stop)(stream), AAUDIO_OK);
            assert_eq!((api.close)(stream), AAUDIO_OK);
        }
    }
}

fn wait_for_streams_on(player: &mut ChildTest, devices: &NullDevices, moment: &str) {
    let wanted = (
        Some(devices.sink.to_string()),
        Some(devices.source.to_string()),
    );
    let deadline = Instant::now() + MOVE_LIMIT;
    loop {
        if !player.running() {
            player.fail(&format!(
                "{moment}: the player ended before its streams reached {wanted:?}"
            ));
        }
        let seen = (
            stream_device(Direction::Output, player.id()),
            stream_device(Direction::Input, player.id()),
        );
        if seen == wanted {
            return;
        }
        if Instant::now() >= deadline {
            player.fail(&format!(
                "{moment}: the streams are on {seen:?}, not {wanted:?}"
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn unplug_and_replug_the_chosen_devices(fallback: &NullDevices) {
    let chosen = NullDevices::create("chosen");
    let routing = routing_to(&chosen);
    let stop = StopFile::new("follow");
    let mut player = run_child_test(FOLLOW_TEST, FOLLOW_PLAYER, stop.0.as_os_str(), |command| {
        routing.apply(command);
    });

    wait_for_streams_on(&mut player, &chosen, "at the start");
    drop(chosen);
    wait_for_streams_on(
        &mut player,
        fallback,
        "after the chosen devices are unplugged",
    );
    let other = NullDevices::create("other");
    std::thread::sleep(SETTLE);
    let chosen = NullDevices::create("chosen");
    wait_for_streams_on(
        &mut player,
        &chosen,
        "after another device appears and the chosen ones are plugged in again",
    );
    chosen.make_system_default();
    std::thread::sleep(SETTLE);
    fallback.make_system_default();
    wait_for_streams_on(
        &mut player,
        &chosen,
        "after the chosen devices were the defaults and stopped being them",
    );

    stop.stop();
    assert_passed(&player.finish());
    drop(other);
}

#[test]
fn streams_follow_a_chosen_device_that_returns_or_stops_being_the_default() {
    if let Some(stop) = std::env::var_os(FOLLOW_PLAYER) {
        play_and_record_reopening_until(Path::new(&stop));
        return;
    }
    on_private_server(FOLLOW_TEST, unplug_and_replug_the_chosen_devices);
}
