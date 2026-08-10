use super::*;
use std::ffi::c_void;

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
    let mut ring = PcmRing::new();
    ring.queue.push_back(vec![0.5; 8]);
    let mut out = [9.0_f32; 4];
    let fired = fill_output(&mut ring, &mut out);
    assert!(fired.is_empty());
    assert_eq!(out, [0.0; 4], "not PLAYING → silence");

    assert_eq!(ring.queued_samples(), 8);
}

#[test]
fn fill_output_drains_and_fires_callback_on_buffer_end() {
    let mut ring = PcmRing::new();
    ring.play_state = SL_PLAYSTATE_PLAYING_TEST();
    ring.queue.push_back(vec![0.1, 0.2, 0.3, 0.4]);

    ring.callback = Some(BufferQueueCallback {
        func: noop_cb,
        context: 0,
        caller: 0,
    });

    let mut out4 = [0.0_f32; 4];
    let fired = fill_output(&mut ring, &mut out4);
    assert_eq!(out4, [0.1, 0.2, 0.3, 0.4]);
    assert!(fired.is_empty(), "buffer consumed but not yet popped");

    let mut out2 = [9.0_f32; 2];
    let fired = fill_output(&mut ring, &mut out2);
    assert_eq!(fired.len(), 1, "one bq-callback fires on buffer end");
    assert_eq!(ring.drained_buffers, 1);
    assert_eq!(ring.callback_fires, 1);
    assert_eq!(out2, [0.0, 0.0], "underrun → silence");
}

#[test]
fn fill_output_spans_multiple_buffers() {
    let mut ring = PcmRing::new();
    ring.play_state = SL_PLAYSTATE_PLAYING_TEST();
    ring.queue.push_back(vec![1.0, 2.0]);
    ring.queue.push_back(vec![3.0, 4.0]);
    let mut out = [0.0_f32; 4];
    let _ = fill_output(&mut ring, &mut out);
    assert_eq!(out, [1.0, 2.0, 3.0, 4.0], "drains across buffer boundary");

    assert_eq!(ring.drained_buffers, 1);
}

extern "C" fn noop_cb(_caller: *mut c_void, _ctx: *mut c_void) {}

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

#[allow(non_snake_case)]
fn SL_PLAYSTATE_PLAYING_TEST() -> u32 {
    SL_PLAYSTATE_PLAYING
}

#[test]
fn itf_offsets_recover_the_owning_object() {
    let off = itf_offsets();
    let sample = Box::new(ObjectState {
        object_vtable: object_vtable(),
        id: 0,
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

    let recovered =
        unsafe { object_from_itf_field(engine_field as *mut c_void, off.engine) } as usize;
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

fn iid_value(index: usize) -> *const c_void {
    let addr = crate::loader::native_provider::sl_iid_addr_for_test(index);

    unsafe { *(addr as *const *const c_void) }
}
