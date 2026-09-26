use super::*;
use std::ffi::{c_char, c_int, CStr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

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

fn renderer<A: cpal::SizedSample>(
    shared: &Arc<StreamShared>,
    callback: DataCallback,
    probe: &CallbackProbe,
) -> Renderer<A> {
    Renderer::new(
        Arc::clone(shared),
        DataCallbackTarget {
            func: callback,
            user_data: probe.user_data(),
        },
        0x1234_0000_0007,
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
fn unspecified_format_reports_the_device_default_rate_channels_and_format() {
    let format = negotiate_format(
        None,
        &device_default(2, 48_000, cpal::SampleFormat::F32),
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
    assert_eq!((format.channels, format.sample_rate), (2, 44_100));
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
    renderer.render_output(&mut out);
    assert_eq!(out, [0.0; 16], "an unstarted stream is silent");
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);

    shared.state.swap(StreamState::Started);
    renderer.render_output(&mut out);
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
    renderer.render_output(&mut out);
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
    renderer.render_output(&mut out);
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
    renderer.render_output(&mut out);
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

    renderer.capture_input(&[0.5f32, -0.5, 1.0, -1.0]);
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0, "not started");

    shared.state.swap(StreamState::Started);
    renderer.capture_input(&[0.5f32, -0.5, 1.0, -1.0]);
    let expected: Vec<i16> = [0.5f32, -0.5, 1.0, -1.0]
        .iter()
        .map(|&s| cpal::Sample::from_sample(s))
        .collect();
    assert_eq!(*probe.received.lock().unwrap(), expected);
}

#[test]
fn host_errors_count_xruns_and_disconnect_once() {
    let shared = Arc::new(StreamShared::new(4));
    shared.state.swap(StreamState::Started);
    let probe = CallbackProbe::default();
    let reporter = ErrorReporter {
        shared: Arc::clone(&shared),
        callback: Some(ErrorCallbackTarget {
            func: record_error,
            user_data: probe.user_data(),
        }),
        stream: 0x10,
    };

    reporter.report(cpal::ErrorKind::Xrun.into());
    reporter.report(cpal::ErrorKind::Xrun.into());
    reporter.report(cpal::Error::with_message(
        cpal::ErrorKind::BackendError,
        "snd_pcm_recover failed",
    ));
    assert_eq!(shared.xrun_count.load(Ordering::Relaxed), 2);
    assert_eq!(shared.state.load(), StreamState::Started);

    reporter.report(cpal::ErrorKind::DeviceNotAvailable.into());
    reporter.report(cpal::ErrorKind::StreamInvalidated.into());
    assert_eq!(shared.state.load(), StreamState::Disconnected);
    assert_eq!(*probe.errors.lock().unwrap(), [AAUDIO_ERROR_DISCONNECTED]);
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

fn default_device_config(direction: Direction) -> Option<cpal::SupportedStreamConfig> {
    let host = cpal::default_host();
    match direction {
        Direction::Output => host.default_output_device()?.default_output_config().ok(),
        Direction::Input => host.default_input_device()?.default_input_config().ok(),
    }
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

#[test]
fn fmod_driver_probe_opens_callback_less_streams_in_both_directions() {
    let api = fmod_view();
    for (direction, raw) in [
        (Direction::Output, AAUDIO_DIRECTION_OUTPUT),
        (Direction::Input, AAUDIO_DIRECTION_INPUT),
    ] {
        let Some(config) = default_device_config(direction) else {
            eprintln!("fmod_driver_probe_opens_callback_less_streams_in_both_directions: no {direction:?} device");
            continue;
        };
        assert_eq!(
            fmod_driver_probe(&api, raw),
            Ok((config.sample_rate() as i32, i32::from(config.channels()))),
            "{direction:?} probe reports the device's own rate and channels"
        );
    }
}

#[test]
fn fmod_output_sequence_plays_through_the_host_device() {
    let api = fmod_view();
    let (probed_rate, probed_channels) = match fmod_driver_probe(&api, AAUDIO_DIRECTION_OUTPUT) {
        Ok(reported) => reported,
        Err(AAUDIO_ERROR_UNAVAILABLE) => {
            eprintln!("fmod_output_sequence_plays_through_the_host_device: no host output device");
            return;
        }
        Err(code) => panic!("driver probe failed with {code}"),
    };
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
