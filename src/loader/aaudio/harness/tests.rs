use super::*;

const RATE: u32 = 48_000;

fn arguments(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

fn millis(value: u64) -> Duration {
    Duration::from_millis(value)
}

#[test]
fn sine_arguments_take_seconds_hz_and_an_optional_stall() {
    assert_eq!(
        SineRequest::parse(&arguments(&["60", "997"])),
        Ok(SineRequest {
            length: Duration::from_secs(60),
            hz: 997,
            stall: None,
        })
    );
    assert_eq!(
        SineRequest::parse(&arguments(&["60", "997", "60", "5"])),
        Ok(SineRequest {
            length: Duration::from_secs(60),
            hz: 997,
            stall: Some(Stall {
                length: millis(60),
                every: Duration::from_secs(5),
            }),
        })
    );
    for wrong in [
        &[][..],
        &["60"],
        &["60", "997", "60"],
        &["0", "997"],
        &["60", "-997"],
        &["60", "997.5"],
        &["60", "997", "60", "5", "1"],
    ] {
        assert_eq!(
            SineRequest::parse(&arguments(wrong)),
            Err(SINE_USAGE.to_owned()),
            "{wrong:?}"
        );
    }
    assert_eq!(
        SineRequest::parse(&arguments(&["60", "24000"])),
        Err(format!("24000 Hz is above 20000 Hz\n{SINE_USAGE}"))
    );
}

#[test]
fn the_tone_callback_writes_one_sine_to_every_channel_across_bursts() {
    let tone = Tone::new(None);
    *lock(&tone.state) = Some(ToneState {
        oscillator: Oscillator::new(997.0, RATE, 2),
        next_stall: None,
    });
    let mut played = Vec::new();
    let mut burst = [f32::NAN; 2 * 511];
    for _ in 0..94 {
        let result = unsafe {
            play_tone(
                std::ptr::null_mut(),
                std::ptr::from_ref(&tone).cast_mut().cast(),
                burst.as_mut_ptr().cast(),
                511,
            )
        };
        assert_eq!(result, AAUDIO_CALLBACK_RESULT_CONTINUE);
        played.extend_from_slice(&burst);
    }
    assert_eq!(tone.callbacks.load(Ordering::Relaxed), 94);
    assert_eq!(tone.stalls.load(Ordering::Relaxed), 0);
    let (left, right): (Vec<f32>, Vec<f32>) = played
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&[left, right]| (left, right))
        .unzip();
    assert_eq!(left, right);
    let peak = left
        .iter()
        .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
    assert!((peak - 0.5).abs() < 1e-3, "peak {peak}");
    let hz = zero_crossing_hz(&left, RATE).expect("a tone");
    assert!((hz - 997.0).abs() < 0.01, "{hz} Hz");
}

#[test]
fn the_tone_callback_leaves_the_buffer_alone_until_the_stream_is_configured() {
    let tone = Tone::new(None);
    let mut burst = [0.25_f32; 8];
    let result = unsafe {
        play_tone(
            std::ptr::null_mut(),
            std::ptr::from_ref(&tone).cast_mut().cast(),
            burst.as_mut_ptr().cast(),
            4,
        )
    };
    assert_eq!(result, AAUDIO_CALLBACK_RESULT_CONTINUE);
    assert_eq!(burst, [0.25; 8]);
}

#[test]
fn stalls_fall_due_once_per_interval_from_the_first_callback() {
    let stall = Stall {
        length: millis(60),
        every: Duration::from_secs(5),
    };
    let start = Instant::now();
    let mut next = None;
    let mut due_at = Vec::new();
    let burst = Duration::from_secs_f64(512.0 / f64::from(RATE));
    let mut at = start;
    while at < start + Duration::from_secs(60) {
        if stall.due(&mut next, at) {
            due_at.push(at.duration_since(start));
            at += stall.length;
        }
        at += burst;
    }
    assert_eq!(due_at.len(), 11, "{due_at:?}");
    for (index, offset) in due_at.iter().enumerate() {
        let wanted = Duration::from_secs(5 * (index as u64 + 1));
        assert!(
            *offset >= wanted && *offset < wanted + millis(11),
            "stall {index} at {offset:?}"
        );
    }
}

#[test]
fn the_click_track_plays_ten_clicks_half_a_second_apart_then_the_tone() {
    let schedule = Schedule::at(RATE);
    let mut track = ClickTrack::new(schedule, 2);
    let start = Instant::now();
    let mut burst = [f32::NAN; 2 * 480];
    let mut mono = Vec::new();
    for index in 0..800_u64 {
        track.render(&mut burst, start + millis(10 * index));
        mono.extend(burst.as_chunks::<2>().0.iter().map(|[left, _]| *left));
    }
    let clicks: Vec<Duration> = track
        .clicks
        .iter()
        .map(|click| click.expect("every click played").duration_since(start))
        .collect();
    let wanted: Vec<Duration> = (0..10).map(|index| millis(500 + 500 * index)).collect();
    assert_eq!(clicks, wanted);
    assert_eq!(
        track.tone.map(|tone| tone.duration_since(start)),
        Some(Duration::from_secs(6))
    );
    let heard = onsets(&mono, DETECTION_LEVEL, usize::MAX);
    assert_eq!(heard.first(), Some(&24_003_usize));
    assert!(mono[..24_000].iter().all(|sample| *sample == 0.0));
    let tone = &mono[288_000..336_000];
    let hz = zero_crossing_hz(tone, RATE).expect("the tone");
    assert!((hz - 997.0).abs() < 0.01, "{hz} Hz");
    assert!(mono[336_000..].iter().all(|sample| *sample == 0.0));
    assert!(schedule.length() >= Duration::from_secs(7));
}

#[test]
fn captured_frames_map_back_to_the_moment_they_were_heard() {
    let start = Instant::now();
    let mut capture = Capture::new(RATE, 2, Duration::from_secs(1));
    capture.record(&[0.0; 2 * 480], start + millis(10));
    capture.record(&[0.0; 2 * 480], start + millis(20));
    assert_eq!(capture.samples.len(), 960);
    let instant = |frame| capture.instant_of(frame).map(|at| at.duration_since(start));
    assert_eq!(instant(479), Some(millis(10)));
    assert_eq!(instant(959), Some(millis(20)));
    let first = instant(0).expect("the first frame");
    assert!(first > millis(0) && first < millis(1), "{first:?}");
    assert_eq!(instant(960), None);
}

#[test]
fn capture_stops_at_its_preallocated_length() {
    let mut capture = Capture::new(RATE, 1, millis(10));
    let room = capture.samples.capacity();
    capture.record(&vec![0.5; room + 100], Instant::now());
    capture.record(&[0.5; 10], Instant::now());
    assert_eq!(capture.samples.len(), room);
    assert_eq!(capture.samples.capacity(), room);
}

#[test]
fn onsets_mark_each_burst_once() {
    let mut samples = vec![0.0_f32; 1_000];
    for start in [100, 130, 600] {
        samples[start..start + 10].fill(0.6);
    }
    samples[900] = -0.9;
    assert_eq!(onsets(&samples, 0.25, 100), [100, 600, 900]);
}

#[test]
fn each_click_pairs_with_the_first_sound_heard_within_its_period() {
    let start = Instant::now();
    let sent = [
        Some(start),
        Some(start + millis(500)),
        None,
        Some(start + millis(1_500)),
    ];
    let heard = [
        start + millis(30),
        start + millis(540),
        start + millis(560),
        start + millis(2_100),
    ];
    assert_eq!(
        round_trips(&sent, &heard, millis(500)),
        [millis(30), millis(40)]
    );
}

#[test]
fn percentiles_take_the_nearest_rank() {
    let delays: Vec<Duration> = (1..=10).map(millis).collect();
    assert_eq!(percentile_ms(&delays, 0.5), Some(5.0));
    assert_eq!(percentile_ms(&delays, 0.95), Some(10.0));
    assert_eq!(percentile_ms(&[], 0.5), None);
}

#[test]
fn zero_crossings_need_a_full_cycle() {
    assert_eq!(zero_crossing_hz(&[0.0; 100], RATE), None);
    assert_eq!(zero_crossing_hz(&[-1.0, 1.0, 1.0], RATE), None);
}

fn loop_back(input_rate: u32, input_channels: usize, delay: Duration) -> Heard {
    let schedule = Schedule::at(RATE);
    let mut track = ClickTrack::new(schedule, 2);
    let mut capture = Capture::new(input_rate, input_channels, schedule.length());
    let burst = 480;
    let decimation = (RATE / input_rate) as usize;
    let mut line = vec![0.0_f32; usize::try_from(frames_in(delay, RATE)).expect("a delay")];
    let mut output = [0.0_f32; 2 * 480];
    let mut input = vec![0.0_f32; input_channels * burst / decimation];
    let start = Instant::now();
    for index in 0..800_u64 {
        track.render(&mut output, start + millis(10 * index));
        line.extend(output.as_chunks::<2>().0.iter().map(|[left, _]| *left));
        for (frame, sample) in input
            .chunks_exact_mut(input_channels)
            .zip(line.drain(..burst).step_by(decimation))
        {
            frame.fill(sample);
        }
        capture.record(&input, start + millis(10 * (index + 1)));
    }
    listen(&track, &capture)
}

#[test]
fn a_loopback_through_a_delay_hears_every_click_and_the_tone_at_its_pitch() {
    let heard = loop_back(RATE, 2, millis(30));
    assert_eq!(heard.round_trips.len(), CLICKS);
    for delay in &heard.round_trips {
        assert!(
            *delay >= millis(30) && *delay < millis(31),
            "round trip {delay:?}"
        );
    }
    let hz = heard.capture_hz.expect("the tone was captured");
    assert!((hz - CAPTURE_TONE_HZ).abs() < 0.01, "{hz} Hz");
}

#[test]
fn a_mono_sixteen_kilohertz_capture_still_hears_the_clicks_and_the_tone_pitch() {
    let heard = loop_back(16_000, 1, millis(30));
    assert_eq!(heard.round_trips.len(), CLICKS);
    for delay in &heard.round_trips {
        assert!(
            *delay >= millis(30) && *delay < millis(31),
            "round trip {delay:?}"
        );
    }
    let hz = heard.capture_hz.expect("the tone was captured");
    assert!((hz - CAPTURE_TONE_HZ).abs() < 0.05, "{hz} Hz");
}

#[test]
fn a_silent_capture_hears_nothing() {
    let heard = loop_back(RATE, 2, Duration::from_secs(10));
    assert_eq!(
        heard,
        Heard {
            round_trips: Vec::new(),
            capture_hz: None,
        }
    );
}

#[test]
fn the_loopback_callbacks_play_clicks_and_keep_the_first_input_channel() {
    let loopback = Loopback::default();
    let user_data = std::ptr::from_ref(&loopback).cast_mut().cast::<c_void>();
    let mut output = [0.5_f32; 2 * 480];
    let mut input = [1.0_f32, 2.0, 3.0, 4.0];
    unsafe {
        assert_eq!(
            play_clicks(
                std::ptr::null_mut(),
                user_data,
                output.as_mut_ptr().cast(),
                480
            ),
            AAUDIO_CALLBACK_RESULT_CONTINUE
        );
        assert_eq!(
            record_input(
                std::ptr::null_mut(),
                user_data,
                input.as_mut_ptr().cast(),
                2
            ),
            AAUDIO_CALLBACK_RESULT_CONTINUE
        );
    }
    assert!(output.iter().all(|sample| *sample == 0.5));

    *lock(&loopback.track) = Some(ClickTrack::new(Schedule::at(RATE), 2));
    *lock(&loopback.capture) = Some(Capture::new(RATE, 2, Duration::from_secs(1)));
    let mut played = Vec::new();
    for _ in 0..60 {
        unsafe {
            play_clicks(
                std::ptr::null_mut(),
                user_data,
                output.as_mut_ptr().cast(),
                480,
            );
        }
        played.extend(output.as_chunks::<2>().0.iter().map(|[left, _]| *left));
    }
    unsafe {
        record_input(
            std::ptr::null_mut(),
            user_data,
            input.as_mut_ptr().cast(),
            2,
        );
    }
    assert_eq!(onsets(&played, DETECTION_LEVEL, usize::MAX), [24_003]);
    let track = lock(&loopback.track).take().expect("the track");
    assert!(track.clicks[0].is_some() && track.clicks[1].is_none());
    let capture = lock(&loopback.capture).take().expect("the capture");
    assert_eq!(capture.samples, [1.0, 3.0]);
    assert_eq!(capture.marks.len(), 1);
}

#[test]
fn reports_are_single_json_lines_with_the_fields_the_checker_reads() {
    let sine = SineReport {
        sample_rate: 48_000,
        channels: 2,
        frames_per_burst: 512,
        callbacks: 5_625,
        xruns: 0,
        stalls: 11,
        stream_errors: 0,
    };
    assert_eq!(
        sine.to_string(),
        r#"{"sample_rate":48000,"channels":2,"frames_per_burst":512,"callbacks":5625,"xruns":0,"stalls":11,"stream_errors":0}"#
    );
    let loopback = LoopbackReport {
        output_rate: 48_000,
        output_channels: 2,
        input_rate: 48_000,
        input_channels: 2,
        clicks: 10,
        detected: 9,
        round_trip_ms_p50: Some(31.5),
        round_trip_ms_p95: None,
        tone_hz: 997.0,
        capture_hz: Some(997.25),
        stream_errors: 1,
    };
    assert_eq!(
        loopback.to_string(),
        r#"{"output_rate":48000,"output_channels":2,"input_rate":48000,"input_channels":2,"clicks":10,"detected":9,"round_trip_ms_p50":31.5,"round_trip_ms_p95":null,"tone_hz":997.0,"capture_hz":997.25,"stream_errors":1}"#
    );
    assert_eq!(Phase::Probe.to_string(), r#"{"phase":"probe"}"#);
    assert_eq!(Phase::Closed.to_string(), r#"{"phase":"closed"}"#);
}
