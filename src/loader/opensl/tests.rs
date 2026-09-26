use super::*;
use std::ffi::c_void;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const MONO_16: PcmFormat = PcmFormat {
    channels: 1,
    sample_rate: 44_100,
    bits_per_sample: 16,
};

#[test]
fn pcm_format_accepts_mono_stereo_8_16_bit() {
    let f = PcmFormat::from_sl_pcm(2, 44_100_000, 16).unwrap();
    assert_eq!(
        f,
        PcmFormat {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 16
        }
    );

    let f = PcmFormat::from_sl_pcm(1, 22_050_000, 8).unwrap();
    assert_eq!(f.channels, 1);
    assert_eq!(f.sample_rate, 22_050);
    assert_eq!(f.bits_per_sample, 8);
}

#[test]
fn pcm_format_rejects_bad_inputs() {
    assert_eq!(
        PcmFormat::from_sl_pcm(0, 44_100_000, 16),
        Err(FormatError::BadChannels)
    );
    assert_eq!(
        PcmFormat::from_sl_pcm(3, 44_100_000, 16),
        Err(FormatError::BadChannels)
    );
    assert_eq!(
        PcmFormat::from_sl_pcm(2, 0, 16),
        Err(FormatError::BadSampleRate)
    );

    assert_eq!(
        PcmFormat::from_sl_pcm(2, 44_100_500, 16),
        Err(FormatError::BadSampleRate)
    );
    assert_eq!(
        PcmFormat::from_sl_pcm(2, 44_100_000, 24),
        Err(FormatError::BadBitsPerSample)
    );
}

#[test]
fn pcm16_converts_signed_le_to_f32() {
    let bytes = [0x00, 0x00, 0xFF, 0x7F, 0x00, 0x80];
    let mut out = Vec::new();
    pcm_to_f32(&bytes, 16, &mut out);
    assert_eq!(out.len(), 3);
    assert!((out[0] - 0.0).abs() < 1e-6);
    assert!((out[1] - 32767.0 / 32768.0).abs() < 1e-6);
    assert!((out[2] - (-1.0)).abs() < 1e-6);
}

#[test]
fn pcm8_converts_unsigned_centered_to_f32() {
    let bytes = [128u8, 255, 0];
    let mut out = Vec::new();
    pcm_to_f32(&bytes, 8, &mut out);
    assert_eq!(out.len(), 3);
    assert!((out[0] - 0.0).abs() < 1e-6);
    assert!((out[1] - 127.0 / 128.0).abs() < 1e-6);
    assert!((out[2] - (-1.0)).abs() < 1e-6);
}

#[test]
fn pcm16_ignores_trailing_partial_sample() {
    let bytes = [0x00, 0x00, 0x11];
    let mut out = Vec::new();
    pcm_to_f32(&bytes, 16, &mut out);
    assert_eq!(out.len(), 1);
}

#[test]
fn generated_sine_is_16bit_le_and_right_length() {
    let bytes = super::generate_sine_pcm16(440.0, 44_100, 100);
    assert_eq!(bytes.len(), 200, "100 frames × 2 bytes (mono 16-bit)");

    let mut out = Vec::new();
    pcm_to_f32(&bytes, 16, &mut out);
    let peak = out.iter().cloned().fold(0.0_f32, |a, b| a.max(b.abs()));
    assert!(peak <= 0.26 && peak > 0.0, "sine peak ~0.25, got {peak}");
}

#[test]
fn fill_output_silence_when_not_playing() {
    let mut ring = PcmRing::new(MONO_16, 2);
    ring.queue.push_back(vec![0.5; 8]);
    let mut out = [9.0_f32; 4];
    let fired = fill_output(&mut ring, &mut out);
    assert_eq!(fired, 0);
    assert_eq!(out, [0.0; 4], "not PLAYING → silence");

    assert_eq!(ring.queued_samples(), 8);
}

#[test]
fn fill_output_drains_and_fires_callback_on_buffer_end() {
    let mut ring = PcmRing::new(MONO_16, 2);
    ring.play_state = SL_PLAYSTATE_PLAYING;
    ring.queue.push_back(vec![0.1, 0.2, 0.3, 0.4]);

    ring.callback = Some(BufferQueueCallback {
        func: noop_cb,
        context: 0,
        caller: 0,
    });

    let mut out4 = [0.0_f32; 4];
    let fired = fill_output(&mut ring, &mut out4);
    assert_eq!(out4, [0.1, 0.2, 0.3, 0.4]);
    assert_eq!(fired, 0, "buffer consumed but not yet popped");

    let mut out2 = [9.0_f32; 2];
    let fired = fill_output(&mut ring, &mut out2);
    assert_eq!(fired, 1, "one bq-callback fires on buffer end");
    assert_eq!(ring.drained_buffers, 1);
    assert_eq!(ring.callback_fires, 1);
    assert_eq!(out2, [0.0, 0.0], "underrun → silence");
}

#[test]
fn fill_output_spans_multiple_buffers() {
    let mut ring = PcmRing::new(MONO_16, 2);
    ring.play_state = SL_PLAYSTATE_PLAYING;
    ring.queue.push_back(vec![1.0, 2.0]);
    ring.queue.push_back(vec![3.0, 4.0]);
    let mut out = [0.0_f32; 4];
    let _ = fill_output(&mut ring, &mut out);
    assert_eq!(out, [1.0, 2.0, 3.0, 4.0], "drains across buffer boundary");

    assert_eq!(ring.drained_buffers, 1);
}

extern "C" fn noop_cb(_caller: *mut c_void, _ctx: *mut c_void) {}

#[test]
fn render_larger_than_the_scratch_matches_one_fill_and_fires_every_drained_buffer() {
    static FIRES: AtomicU32 = AtomicU32::new(0);
    extern "C" fn count_fire(_caller: *mut c_void, _ctx: *mut c_void) {
        FIRES.fetch_add(1, Ordering::SeqCst);
    }
    let playing_ring = || {
        let mut ring = PcmRing::new(MONO_16, 4);
        ring.play_state = SL_PLAYSTATE_PLAYING;
        ring.volume_level_mb = -600;
        ring.callback = Some(BufferQueueCallback {
            func: count_fire,
            context: 0,
            caller: 0,
        });
        for len in [3_001, RENDER_CHUNK_SAMPLES, 1_703] {
            ring.queue
                .push_back((0..len).map(|i| (i % 97) as f32 / 97.0).collect());
        }
        ring
    };
    let total = 3 * RENDER_CHUNK_SAMPLES + 11;

    let mut reference_ring = playing_ring();
    let mut expected = vec![0.0f32; total];
    let expected_fires = fill_output(&mut reference_ring, &mut expected);

    let ring = Arc::new(Mutex::new(playing_ring()));
    let mut renderer = RingRenderer::new(Arc::clone(&ring));
    let mut out = vec![9.0f32; total];
    renderer.render(&mut out);

    assert_eq!(out, expected, "chunked rendering is sample-identical");
    assert_eq!(expected_fires, 3);
    assert_eq!(FIRES.load(Ordering::SeqCst), expected_fires);
    assert_eq!(
        ring.lock().unwrap().drained_buffers,
        reference_ring.drained_buffers
    );
}

#[test]
fn public_opensl_constants_match_khronos_abi() {
    assert_eq!(SL_RESULT_PRECONDITIONS_VIOLATED, 1);
    assert_eq!(SL_RESULT_PARAMETER_INVALID, 2);
    assert_eq!(SL_RESULT_MEMORY_FAILURE, 3);
    assert_eq!(SL_RESULT_BUFFER_INSUFFICIENT, 7);
    assert_eq!(SL_RESULT_FEATURE_UNSUPPORTED, 12);
    assert_eq!(SL_DATALOCATOR_OUTPUTMIX, 4);
    assert_eq!(SL_DATALOCATOR_BUFFERQUEUE, 6);
    assert_eq!(SL_DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE, 0x8000_07BD);
}

#[test]
fn itf_offsets_recover_the_owning_object() {
    let off = itf_offsets();
    let sample = Box::new(ObjectState {
        object_vtable: object_vtable(),
        engine_itf: engine_vtable(),
        play_itf: std::ptr::null(),
        bufferqueue_itf: std::ptr::null(),
        volume_itf: std::ptr::null(),
        android_config_itf: std::ptr::null(),
        state: 0,
        kind: ObjectKind::OutputMix,
    });
    let base = sample.as_ref() as *const ObjectState as *const u8 as usize;
    let engine_field = std::ptr::addr_of!(sample.engine_itf) as usize;
    assert_eq!(engine_field - base, off.engine);

    let recovered = object_from_itf_field(engine_field as *mut c_void, off.engine) as usize;
    assert_eq!(recovered, base, "object_from_itf_field round-trips");
    assert_eq!(
        std::ptr::addr_of!(sample.volume_itf) as usize - base,
        off.volume
    );
    assert_eq!(
        std::ptr::addr_of!(sample.android_config_itf) as usize - base,
        off.android_config
    );
}

#[test]
fn stale_handle_after_destroy_is_rejected_not_ub() {
    let mut engine: *mut c_void = std::ptr::null_mut();

    let r = unsafe {
        eclipse_sl_create_engine(
            std::ptr::addr_of_mut!(engine).cast(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(r, SL_RESULT_SUCCESS);
    assert!(!engine.is_null());

    assert_eq!(obj_realize(engine, 0), SL_RESULT_SUCCESS);

    obj_destroy(engine);

    let mut state = 0u32;
    let r = obj_get_state(engine, &mut state);
    assert_eq!(
        r, SL_RESULT_PARAMETER_INVALID,
        "a destroyed handle is rejected, never dereferenced wildly"
    );
    obj_destroy(engine);
    assert_eq!(obj_realize(engine, 0), SL_RESULT_PARAMETER_INVALID);
}

#[test]
fn handle_identity_comes_from_the_registry_not_from_handle_memory() {
    let mut engine: *mut c_void = std::ptr::null_mut();
    let r = unsafe {
        eclipse_sl_create_engine(
            std::ptr::addr_of_mut!(engine).cast(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(r, SL_RESULT_SUCCESS);
    assert_eq!(obj_realize(engine, 0), SL_RESULT_SUCCESS);

    let mut copy = MaybeUninit::<ObjectState>::uninit();
    unsafe {
        std::ptr::copy_nonoverlapping(engine as *const ObjectState, copy.as_mut_ptr(), 1);
    }
    let forged: SlObjectItf = copy.as_mut_ptr().cast();

    let mut state = 0u32;
    assert_eq!(
        obj_get_state(forged, &mut state),
        SL_RESULT_PARAMETER_INVALID,
        "a byte-identical copy at another address is not a live object"
    );
    obj_destroy(forged);
    assert_eq!(obj_get_state(engine, &mut state), SL_RESULT_SUCCESS);
    assert_eq!(state, SL_OBJECT_STATE_REALIZED);
    obj_destroy(engine);
}

#[test]
fn host_stream_config_honours_the_source_rate_and_channels() {
    let range = |channels, min, max, format| {
        cpal::SupportedStreamConfigRange::new(
            channels,
            min,
            max,
            cpal::SupportedBufferSize::Unknown,
            format,
        )
    };
    let wide = [
        range(2, 1, 384_000, cpal::SampleFormat::F32),
        range(1, 1, 384_000, cpal::SampleFormat::F32),
    ];
    let mono_48k = PcmFormat {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 16,
    };
    let config = host_stream_config(mono_48k, wide.into_iter()).unwrap();
    assert_eq!((config.channels(), config.sample_rate()), (1, 48_000));

    let stereo_22k = PcmFormat {
        channels: 2,
        sample_rate: 22_050,
        bits_per_sample: 16,
    };
    let config = host_stream_config(stereo_22k, wide.into_iter()).unwrap();
    assert_eq!((config.channels(), config.sample_rate()), (2, 22_050));

    assert_eq!(
        host_stream_config(
            mono_48k,
            [range(2, 1, 384_000, cpal::SampleFormat::F32)].into_iter()
        )
        .err(),
        Some(AudioHostError::UnsupportedFormat)
    );

    let stereo_48k = PcmFormat {
        channels: 2,
        sample_rate: 48_000,
        bits_per_sample: 16,
    };
    let config = host_stream_config(
        stereo_48k,
        [
            range(2, 44_100, 44_100, cpal::SampleFormat::F32),
            range(2, 8_000, 96_000, cpal::SampleFormat::I16),
        ]
        .into_iter(),
    )
    .unwrap();
    assert_eq!(config.sample_format(), cpal::SampleFormat::I16);
    assert_eq!(config.sample_rate(), 48_000);
}

#[test]
fn host_stream_config_opens_i32_and_i24_only_devices() {
    let stereo_48k = PcmFormat {
        channels: 2,
        sample_rate: 48_000,
        bits_per_sample: 16,
    };
    for format in [cpal::SampleFormat::I32, cpal::SampleFormat::I24] {
        let only = cpal::SupportedStreamConfigRange::new(
            2,
            8_000,
            96_000,
            cpal::SupportedBufferSize::Unknown,
            format,
        );
        let config = host_stream_config(stereo_48k, [only].into_iter()).unwrap();
        assert_eq!(config.sample_format(), format);
        assert_eq!((config.channels(), config.sample_rate()), (2, 48_000));
    }
}

#[test]
fn render_converts_the_ring_to_i32_and_i24_device_samples() {
    let ring = Arc::new(Mutex::new(PcmRing::new(MONO_16, 2)));
    {
        let mut guard = ring.lock().unwrap();
        guard.play_state = SL_PLAYSTATE_PLAYING;
        guard.enqueue(&[0x00, 0x40, 0x00, 0xC0]);
        guard.enqueue(&[0x00, 0x40, 0x00, 0xC0]);
    }
    let mut renderer = RingRenderer::new(ring);

    let mut wide = [9_i32; 2];
    renderer.render(&mut wide);
    assert_eq!(wide, [1 << 30, -(1 << 30)]);

    let mut packed = [<cpal::I24 as cpal::Sample>::EQUILIBRIUM; 2];
    renderer.render(&mut packed);
    assert_eq!(packed.map(|sample| sample.inner()), [1 << 22, -(1 << 22)]);
}

#[test]
fn enqueue_respects_the_queue_depth_and_recycles_drained_buffers() {
    let mut ring = PcmRing::new(MONO_16, 2);
    let pcm = [0u8, 0, 0, 0x40, 0, 0xC0];
    assert_eq!(ring.enqueue(&pcm), SL_RESULT_SUCCESS);
    assert_eq!(ring.enqueue(&pcm), SL_RESULT_SUCCESS);
    assert_eq!(
        ring.enqueue(&pcm),
        SL_RESULT_BUFFER_INSUFFICIENT,
        "a full simple buffer queue rejects the buffer"
    );
    assert_eq!(ring.enqueue(&[]), SL_RESULT_BUFFER_INSUFFICIENT);

    ring.play_state = SL_PLAYSTATE_PLAYING;
    let first = ring.queue[0].as_ptr();
    let mut out = [0.0f32; 4];
    fill_output(&mut ring, &mut out);
    assert_eq!(ring.spare.len(), 1, "the drained buffer is kept for reuse");
    assert_eq!(ring.enqueue(&pcm), SL_RESULT_SUCCESS);
    assert!(ring.spare.is_empty());
    assert_eq!(
        ring.queue.back().map(|b| b.as_ptr()),
        Some(first),
        "enqueue reuses the drained allocation"
    );
    assert_eq!(ring.queue.capacity(), 2);
    assert_eq!(ring.spare.capacity(), 2);

    ring.clear();
    assert!(ring.queue.is_empty());
    assert_eq!(ring.spare.len(), 2);
    assert_eq!(ring.enqueue(&[]), SL_RESULT_PARAMETER_INVALID);
    assert_eq!(ring.spare.len(), 2, "a rejected buffer returns to the pool");
}

#[test]
fn null_handle_methods_return_parameter_invalid() {
    let mut state = 0u32;
    assert_eq!(
        obj_get_state(std::ptr::null_mut(), &mut state),
        SL_RESULT_PARAMETER_INVALID
    );
    assert_eq!(
        obj_realize(std::ptr::null_mut(), 0),
        SL_RESULT_PARAMETER_INVALID
    );
}

#[test]
fn full_engine_path_builds_and_enqueues_with_zero_sl_errors_no_device() {
    let mut engine: *mut c_void = std::ptr::null_mut();

    assert_eq!(
        unsafe {
            eclipse_sl_create_engine(
                std::ptr::addr_of_mut!(engine).cast(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                std::ptr::null(),
            )
        },
        SL_RESULT_SUCCESS
    );
    assert_eq!(obj_realize(engine, 0), SL_RESULT_SUCCESS);

    let iid_engine = iid_value(3);
    let mut eng_itf: *mut c_void = std::ptr::null_mut();
    assert_eq!(
        obj_get_interface(engine, iid_engine, std::ptr::addr_of_mut!(eng_itf).cast()),
        SL_RESULT_SUCCESS
    );
    assert!(!eng_itf.is_null());

    let mut mix: *mut c_void = std::ptr::null_mut();

    let r = unsafe {
        let vt = *(eng_itf as *const *const EngineItfVtable);
        ((*vt).create_output_mix)(
            eng_itf,
            std::ptr::addr_of_mut!(mix).cast(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(r, SL_RESULT_SUCCESS);
    assert!(!mix.is_null());
    assert_eq!(obj_realize(mix, 0), SL_RESULT_SUCCESS);

    let mut bq_loc = SlDataLocatorBufferQueue {
        locator_type: SL_DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE,
        num_buffers: 2,
    };
    let mut pcm = SlDataFormatPcm {
        format_type: SL_DATAFORMAT_PCM,
        num_channels: 1,
        samples_per_sec: 44_100_000,
        bits_per_sample: 16,
        container_size: 16,
        channel_mask: 0,
        endianness: 0,
    };
    let src = SlDataSource {
        p_locator: std::ptr::addr_of_mut!(bq_loc).cast(),
        p_format: std::ptr::addr_of_mut!(pcm).cast(),
    };
    let mut mix_loc = SlDataLocatorOutputMix {
        locator_type: SL_DATALOCATOR_OUTPUTMIX,
        output_mix: mix,
    };
    let snk = SlDataSink {
        p_locator: std::ptr::addr_of_mut!(mix_loc).cast(),
        p_format: std::ptr::null(),
    };
    let mut player: *mut c_void = std::ptr::null_mut();

    let requested = [iid_value(2), iid_value(6), iid_value(0)];
    let required = [SL_BOOLEAN_TRUE; 3];

    let r = unsafe {
        let vt = *(eng_itf as *const *const EngineItfVtable);
        ((*vt).create_audio_player)(
            eng_itf,
            std::ptr::addr_of_mut!(player).cast(),
            std::ptr::addr_of!(src) as *mut c_void,
            std::ptr::addr_of!(snk) as *mut c_void,
            requested.len() as u32,
            requested.as_ptr().cast(),
            required.as_ptr().cast(),
        )
    };
    assert_eq!(r, SL_RESULT_SUCCESS);
    assert!(!player.is_null());

    let mut config_itf: *mut c_void = std::ptr::null_mut();
    assert_eq!(
        obj_get_interface(
            player,
            iid_value(0),
            std::ptr::addr_of_mut!(config_itf).cast()
        ),
        SL_RESULT_SUCCESS
    );
    let performance_key = b"androidPerformanceMode\0";
    let requested_mode = 0u32;

    let r = unsafe {
        let vt = *(config_itf as *const *const AndroidConfigurationItfVtable);
        ((*vt).set_configuration)(
            config_itf,
            performance_key.as_ptr().cast(),
            std::ptr::addr_of!(requested_mode).cast(),
            std::mem::size_of_val(&requested_mode) as u32,
        )
    };
    assert_eq!(r, SL_RESULT_SUCCESS);

    let mut early_play: *mut c_void = std::ptr::null_mut();
    assert_eq!(
        obj_get_interface(
            player,
            iid_value(4),
            std::ptr::addr_of_mut!(early_play).cast()
        ),
        SL_RESULT_PRECONDITIONS_VIOLATED
    );
    assert_eq!(obj_realize(player, 0), SL_RESULT_SUCCESS);

    let mut play_itf: *mut c_void = std::ptr::null_mut();
    assert_eq!(
        obj_get_interface(
            player,
            iid_value(4),
            std::ptr::addr_of_mut!(play_itf).cast()
        ),
        SL_RESULT_SUCCESS
    );
    let mut bq_itf: *mut c_void = std::ptr::null_mut();
    assert_eq!(
        obj_get_interface(player, iid_value(1), std::ptr::addr_of_mut!(bq_itf).cast()),
        SL_RESULT_SUCCESS
    );
    let mut volume_itf: *mut c_void = std::ptr::null_mut();
    assert_eq!(
        obj_get_interface(
            player,
            iid_value(6),
            std::ptr::addr_of_mut!(volume_itf).cast()
        ),
        SL_RESULT_SUCCESS
    );

    let mut level = -1i16;

    let r = unsafe {
        let vt = *(volume_itf as *const *const VolumeItfVtable);
        ((*vt).get_volume_level)(volume_itf, &mut level)
    };
    assert_eq!(r, SL_RESULT_SUCCESS);
    assert_eq!(level, 0);

    let r = unsafe {
        let vt = *(play_itf as *const *const PlayItfVtable);
        ((*vt).set_play_state)(play_itf, SL_PLAYSTATE_PLAYING)
    };
    assert_eq!(r, SL_RESULT_SUCCESS);

    let bytes = super::generate_sine_pcm16(440.0, 44_100, 256);

    let r = unsafe {
        let vt = *(bq_itf as *const *const BufferQueueItfVtable);
        ((*vt).enqueue)(bq_itf, bytes.as_ptr() as *const c_void, bytes.len() as u32)
    };
    assert_eq!(r, SL_RESULT_SUCCESS, "Enqueue must succeed (0 SL errors)");

    let mut bqs = SlBufferQueueState { count: 0, index: 0 };

    let r = unsafe {
        let vt = *(bq_itf as *const *const BufferQueueItfVtable);
        ((*vt).get_state)(bq_itf, &mut bqs)
    };
    assert_eq!(r, SL_RESULT_SUCCESS);
    assert!(
        bqs.count >= 1,
        "the enqueued buffer is queued (or already draining on a device)"
    );

    let mut rec_itf: *mut c_void = std::ptr::null_mut();
    assert_eq!(
        obj_get_interface(player, iid_value(5), std::ptr::addr_of_mut!(rec_itf).cast()),
        SL_RESULT_PARAMETER_INVALID
    );

    obj_destroy(player);
    obj_destroy(mix);
    obj_destroy(engine);
}

#[test]
fn create_audio_player_rejects_non_pcm_source() {
    let mut engine: *mut c_void = std::ptr::null_mut();

    unsafe {
        eclipse_sl_create_engine(
            std::ptr::addr_of_mut!(engine).cast(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        );
    }
    obj_realize(engine, 0);
    let mut eng_itf: *mut c_void = std::ptr::null_mut();
    obj_get_interface(engine, iid_value(3), std::ptr::addr_of_mut!(eng_itf).cast());

    let mut bad_loc: u32 = SL_DATALOCATOR_OUTPUTMIX;
    let mut pcm = SlDataFormatPcm {
        format_type: SL_DATAFORMAT_PCM,
        num_channels: 1,
        samples_per_sec: 44_100_000,
        bits_per_sample: 16,
        container_size: 16,
        channel_mask: 0,
        endianness: 0,
    };
    let src = SlDataSource {
        p_locator: std::ptr::addr_of_mut!(bad_loc).cast(),
        p_format: std::ptr::addr_of_mut!(pcm).cast(),
    };
    let mut mix_loc = SlDataLocatorOutputMix {
        locator_type: SL_DATALOCATOR_OUTPUTMIX,
        output_mix: std::ptr::null_mut(),
    };
    let snk = SlDataSink {
        p_locator: std::ptr::addr_of_mut!(mix_loc).cast(),
        p_format: std::ptr::null(),
    };
    let mut player: *mut c_void = std::ptr::null_mut();

    let r = unsafe {
        let vt = *(eng_itf as *const *const EngineItfVtable);
        ((*vt).create_audio_player)(
            eng_itf,
            std::ptr::addr_of_mut!(player).cast(),
            std::ptr::addr_of!(src) as *mut c_void,
            std::ptr::addr_of!(snk) as *mut c_void,
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(
        r, SL_RESULT_FEATURE_UNSUPPORTED,
        "non-buffer-queue source → unsupported"
    );
    assert!(player.is_null(), "no player object on failure");
    obj_destroy(engine);
}

struct TestPlayer {
    engine: SlObjectItf,
    mix: SlObjectItf,
    player: SlObjectItf,
}

impl TestPlayer {
    fn create() -> Self {
        let mut engine: *mut c_void = std::ptr::null_mut();
        let r = unsafe {
            eclipse_sl_create_engine(
                std::ptr::addr_of_mut!(engine).cast(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(r, SL_RESULT_SUCCESS);
        assert_eq!(obj_realize(engine, 0), SL_RESULT_SUCCESS);
        let eng_itf = interface(engine, 3);

        let mut mix: *mut c_void = std::ptr::null_mut();
        let r = unsafe {
            let vt = *(eng_itf as *const *const EngineItfVtable);
            ((*vt).create_output_mix)(
                eng_itf,
                std::ptr::addr_of_mut!(mix).cast(),
                0,
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(r, SL_RESULT_SUCCESS);
        assert_eq!(obj_realize(mix, 0), SL_RESULT_SUCCESS);

        let mut bq_loc = SlDataLocatorBufferQueue {
            locator_type: SL_DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE,
            num_buffers: 2,
        };
        let mut pcm = SlDataFormatPcm {
            format_type: SL_DATAFORMAT_PCM,
            num_channels: 1,
            samples_per_sec: 44_100_000,
            bits_per_sample: 16,
            container_size: 16,
            channel_mask: 0,
            endianness: 0,
        };
        let src = SlDataSource {
            p_locator: std::ptr::addr_of_mut!(bq_loc).cast(),
            p_format: std::ptr::addr_of_mut!(pcm).cast(),
        };
        let mut mix_loc = SlDataLocatorOutputMix {
            locator_type: SL_DATALOCATOR_OUTPUTMIX,
            output_mix: mix,
        };
        let snk = SlDataSink {
            p_locator: std::ptr::addr_of_mut!(mix_loc).cast(),
            p_format: std::ptr::null(),
        };
        let mut player: *mut c_void = std::ptr::null_mut();
        let r = unsafe {
            let vt = *(eng_itf as *const *const EngineItfVtable);
            ((*vt).create_audio_player)(
                eng_itf,
                std::ptr::addr_of_mut!(player).cast(),
                std::ptr::addr_of!(src) as *mut c_void,
                std::ptr::addr_of!(snk) as *mut c_void,
                0,
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(r, SL_RESULT_SUCCESS);
        assert_eq!(obj_realize(player, 0), SL_RESULT_SUCCESS);
        Self {
            engine,
            mix,
            player,
        }
    }

    fn bq(&self) -> *mut c_void {
        interface(self.player, 1)
    }

    fn play(&self) {
        let play_itf = interface(self.player, 4);
        let r = unsafe {
            let vt = *(play_itf as *const *const PlayItfVtable);
            ((*vt).set_play_state)(play_itf, SL_PLAYSTATE_PLAYING)
        };
        assert_eq!(r, SL_RESULT_SUCCESS);
    }

    fn destroy_rest(self) {
        obj_destroy(self.mix);
        obj_destroy(self.engine);
    }
}

fn interface(obj: SlObjectItf, index: usize) -> *mut c_void {
    let mut itf: *mut c_void = std::ptr::null_mut();
    assert_eq!(
        obj_get_interface(obj, iid_value(index), std::ptr::addr_of_mut!(itf).cast()),
        SL_RESULT_SUCCESS
    );
    itf
}

fn bq_enqueue_via_vtable(bq: *mut c_void, bytes: &[u8]) -> u32 {
    unsafe {
        let vt = *(bq as *const *const BufferQueueItfVtable);
        ((*vt).enqueue)(bq, bytes.as_ptr().cast(), bytes.len() as u32)
    }
}

#[test]
fn enqueue_from_a_buffer_callback_does_not_take_the_registry_lock() {
    static RESULT: AtomicU32 = AtomicU32::new(u32::MAX);
    extern "C" fn refill(caller: *mut c_void, _ctx: *mut c_void) {
        RESULT.store(
            bq_enqueue_via_vtable(caller, &[0, 0, 0, 0]),
            Ordering::SeqCst,
        );
    }

    let test_player = TestPlayer::create();
    let bq = test_player.bq();
    let r = unsafe {
        let vt = *(bq as *const *const BufferQueueItfVtable);
        ((*vt).register_callback)(bq, refill as *const c_void, std::ptr::null_mut())
    };
    assert_eq!(r, SL_RESULT_SUCCESS);
    let (ring, callback) = with_player_state(test_player.player, |p| {
        let ring = Arc::clone(&p.ring);
        let callback = ring.lock().unwrap().callback.expect("registered");
        (ring, callback)
    })
    .unwrap();

    let (tx, rx) = mpsc::channel();
    let registry_guard = registry().lock().unwrap();
    std::thread::spawn(move || {
        fire_buffer_callbacks(&ring, callback, 1);
        tx.send(()).unwrap();
    });
    let finished = rx.recv_timeout(Duration::from_secs(5));
    drop(registry_guard);
    assert!(
        finished.is_ok(),
        "the refill completed while the registry was locked"
    );
    assert_eq!(RESULT.load(Ordering::SeqCst), SL_RESULT_SUCCESS);

    obj_destroy(test_player.player);
    test_player.destroy_rest();
}

#[test]
fn destroy_while_a_buffer_callback_waits_on_the_registry_does_not_deadlock() {
    static PLAYER: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static ENTERED: AtomicBool = AtomicBool::new(false);
    static DESTROYING: AtomicBool = AtomicBool::new(false);
    static RESULT: AtomicU32 = AtomicU32::new(u32::MAX);
    extern "C" fn query_destroyed_player(_caller: *mut c_void, _ctx: *mut c_void) {
        if ENTERED.swap(true, Ordering::SeqCst) {
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while !DESTROYING.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::yield_now();
        }
        let player = PLAYER.load(Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(reg) = registry().try_lock() {
                if reg.live_slot(player.cast()).is_err() {
                    break;
                }
            }
            std::thread::yield_now();
        }
        let mut state = 0u32;
        RESULT.store(obj_get_state(player, &mut state), Ordering::SeqCst);
    }

    let test_player = TestPlayer::create();
    if !player_has_host_stream(test_player.player).unwrap_or(false) {
        eprintln!(
            "destroy_while_a_buffer_callback_waits_on_the_registry_does_not_deadlock: no device"
        );
        obj_destroy(test_player.player);
        test_player.destroy_rest();
        return;
    }
    PLAYER.store(test_player.player, Ordering::SeqCst);
    let bq = test_player.bq();
    let r = unsafe {
        let vt = *(bq as *const *const BufferQueueItfVtable);
        ((*vt).register_callback)(
            bq,
            query_destroyed_player as *const c_void,
            std::ptr::null_mut(),
        )
    };
    assert_eq!(r, SL_RESULT_SUCCESS);
    test_player.play();
    assert_eq!(bq_enqueue_via_vtable(bq, &[0; 64]), SL_RESULT_SUCCESS);

    let deadline = Instant::now() + Duration::from_secs(3);
    while !ENTERED.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        ENTERED.load(Ordering::SeqCst),
        "the host stream drained the buffer"
    );

    let (tx, rx) = mpsc::channel();
    let player_addr = test_player.player as usize;
    std::thread::spawn(move || {
        DESTROYING.store(true, Ordering::SeqCst);
        destroy_object_for_test(player_addr as *mut c_void);
        tx.send(()).unwrap();
    });
    assert!(
        rx.recv_timeout(Duration::from_secs(5)).is_ok(),
        "Destroy must not hold the registry while it waits for the callback"
    );
    assert_eq!(
        RESULT.load(Ordering::SeqCst),
        SL_RESULT_PARAMETER_INVALID,
        "the callback's registry call returned and saw the player gone"
    );
    test_player.destroy_rest();
}

fn iid_value(index: usize) -> *const c_void {
    let addr = crate::loader::native_provider::sl_iid_addr_for_test(index);

    unsafe { *(addr as *const *const c_void) }
}
