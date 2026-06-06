//! GPU/VM-free unit tests for the OpenSL ES engine: PCM validation + format conversion + ring
//! bookkeeping + object-registry soundness + the full create→mix→player→Enqueue path (no device).
//!
//! 2026-06-05: none of these need a host audio device — they exercise the pure logic (the SL result/
//! handle/vtable management, the buffer-queue bookkeeping, the format conversion). The device-driven
//! end-to-end check is the gated `eclipse __audio-test` harness ([`super::run_audio_test`]).

use super::*;
use std::ffi::c_void;

// ---- PCM format validation -------------------------------------------------------------------

#[test]
fn pcm_format_accepts_mono_stereo_8_16_bit() {
    // 16-bit stereo 44.1 kHz (milliHz → Hz).
    let f = PcmFormat::from_sl_pcm(2, 44_100_000, 16).unwrap();
    assert_eq!(
        f,
        PcmFormat {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 16
        }
    );
    // 8-bit mono 22.05 kHz.
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
    // Not a whole Hz (milliHz not divisible by 1000).
    assert_eq!(
        PcmFormat::from_sl_pcm(2, 44_100_500, 16),
        Err(FormatError::BadSampleRate)
    );
    assert_eq!(
        PcmFormat::from_sl_pcm(2, 44_100_000, 24),
        Err(FormatError::BadBitsPerSample)
    );
}

// ---- PCM → f32 conversion --------------------------------------------------------------------

#[test]
fn pcm16_converts_signed_le_to_f32() {
    // 0x0000 → 0.0 ; 0x7FFF → ~1.0 (32767/32768) ; 0x8000 (-32768) → -1.0.
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
    // 128 → 0.0 ; 255 → ~+1 ; 0 → -1.
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
    // 3 bytes = one whole 16-bit sample + a dangling byte (ignored; total, no panic).
    let bytes = [0x00, 0x00, 0x11];
    let mut out = Vec::new();
    pcm_to_f32(&bytes, 16, &mut out);
    assert_eq!(out.len(), 1);
}

#[test]
fn generated_sine_is_16bit_le_and_right_length() {
    let bytes = super::generate_sine_pcm16(440.0, 44_100, 100);
    assert_eq!(bytes.len(), 200, "100 frames × 2 bytes (mono 16-bit)");
    // Decode back; the peak amplitude must be within the 0.25 generation amplitude.
    let mut out = Vec::new();
    pcm_to_f32(&bytes, 16, &mut out);
    let peak = out.iter().cloned().fold(0.0_f32, |a, b| a.max(b.abs()));
    assert!(peak <= 0.26 && peak > 0.0, "sine peak ~0.25, got {peak}");
}

// ---- PcmRing / fill_output bookkeeping -------------------------------------------------------

#[test]
fn fill_output_silence_when_not_playing() {
    let mut ring = PcmRing::new(); // STOPPED
    ring.queue.push_back(vec![0.5; 8]);
    let mut out = [9.0_f32; 4];
    let fired = fill_output(&mut ring, &mut out);
    assert!(fired.is_empty());
    assert_eq!(out, [0.0; 4], "not PLAYING → silence");
    // The queued buffer is untouched (nothing drained).
    assert_eq!(ring.queued_samples(), 8);
}

#[test]
fn fill_output_drains_and_fires_callback_on_buffer_end() {
    let mut ring = PcmRing::new();
    ring.play_state = SL_PLAYSTATE_PLAYING_TEST();
    ring.queue.push_back(vec![0.1, 0.2, 0.3, 0.4]); // 4 samples
                                                    // Register a callback so we observe the fire.
    ring.callback = Some(BufferQueueCallback {
        func: noop_cb,
        context: 0,
        caller: 0,
    });
    // Pull 4 samples: exactly empties the buffer, but the drain/fire happens on the NEXT poll that
    // finds front_pos >= len. So first poll consumes all 4, then a second poll drains+fires.
    let mut out4 = [0.0_f32; 4];
    let fired = fill_output(&mut ring, &mut out4);
    assert_eq!(out4, [0.1, 0.2, 0.3, 0.4]);
    assert!(fired.is_empty(), "buffer consumed but not yet popped");
    // Second poll: front fully consumed → pop + fire + then underrun silence.
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
    // The first buffer's end was reached mid-fill, so it popped + (no callback registered) drained++.
    assert_eq!(ring.drained_buffers, 1);
}

extern "C" fn noop_cb(_caller: *mut c_void, _ctx: *mut c_void) {}

// A tiny accessor so the test can set PLAYING without exposing the const publicly.
#[allow(non_snake_case)]
fn SL_PLAYSTATE_PLAYING_TEST() -> u32 {
    SL_PLAYSTATE_PLAYING
}

// ---- itf-offset layout consistency -----------------------------------------------------------

#[test]
fn itf_offsets_recover_the_owning_object() {
    // Build a real object, compute the offsets, then prove subtracting the offset from a field
    // address recovers the base — the invariant GetInterface/interface-methods rely on.
    let off = itf_offsets();
    let sample = Box::new(ObjectState {
        object_vtable: object_vtable(),
        id: 0,
        engine_itf: engine_vtable(),
        play_itf: std::ptr::null(),
        bufferqueue_itf: std::ptr::null(),
        state: 0,
        kind: ObjectKind::OutputMix,
    });
    let base = sample.as_ref() as *const ObjectState as *const u8 as usize;
    let engine_field = std::ptr::addr_of!(sample.engine_itf) as usize;
    assert_eq!(engine_field - base, off.engine);
    // SAFETY: recover the base from the field address via the documented offset.
    let recovered =
        unsafe { object_from_itf_field(engine_field as *mut c_void, off.engine) } as usize;
    assert_eq!(recovered, base, "object_from_itf_field round-trips");
}

// ---- Object registry soundness ---------------------------------------------------------------

#[test]
fn stale_handle_after_destroy_is_rejected_not_ub() {
    // Create an engine, destroy it, then call a method on the stale handle → typed error sentinel,
    // never UB.
    let mut engine: *mut c_void = std::ptr::null_mut();
    // SAFETY: valid out-param.
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
    // Realize works while live.
    assert_eq!(obj_realize(engine, 0), SL_RESULT_SUCCESS);
    // Destroy frees the slot (bumps generation).
    obj_destroy(engine);
    // The same handle now refers to a freed slot → GetState rejects it (not UB).
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

// ---- Full create→mix→player→Enqueue path WITHOUT a host device -------------------------------

#[test]
fn full_engine_path_builds_and_enqueues_with_zero_sl_errors_no_device() {
    // Drive the exact vtable path the engine uses, asserting 0 SL errors at every step. This works
    // whether or not a host device exists (the player accepts Enqueues either way); it proves the
    // SLObjectItf/itf vtable management + the PCM bookkeeping without needing audio hardware.

    // 1) engine
    let mut engine: *mut c_void = std::ptr::null_mut();
    // SAFETY: valid out-param.
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

    // GetInterface(SL_IID_ENGINE) → engine itf.
    let iid_engine = iid_value(3);
    let mut eng_itf: *mut c_void = std::ptr::null_mut();
    assert_eq!(
        obj_get_interface(engine, iid_engine, std::ptr::addr_of_mut!(eng_itf).cast()),
        SL_RESULT_SUCCESS
    );
    assert!(!eng_itf.is_null());

    // 2) output mix
    let mut mix: *mut c_void = std::ptr::null_mut();
    // SAFETY: eng_itf is the engine itf.
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

    // 3) audio player (buffer-queue PCM source → output-mix sink)
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
    // SAFETY: eng_itf is the engine itf; src/snk are valid for the call.
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
    assert!(!player.is_null());
    assert_eq!(obj_realize(player, 0), SL_RESULT_SUCCESS);

    // GetInterface(PLAY) + GetInterface(BUFFERQUEUE).
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

    // SetPlayState(PLAYING).
    // SAFETY: play_itf is the player's play itf.
    let r = unsafe {
        let vt = *(play_itf as *const *const PlayItfVtable);
        ((*vt).set_play_state)(play_itf, SL_PLAYSTATE_PLAYING)
    };
    assert_eq!(r, SL_RESULT_SUCCESS);

    // Enqueue a sine PCM buffer → SUCCESS; the ring now holds the decoded samples (device or not).
    let bytes = super::generate_sine_pcm16(440.0, 44_100, 256);
    // SAFETY: bq_itf is the bq itf; the PCM slice is valid for the call.
    let r = unsafe {
        let vt = *(bq_itf as *const *const BufferQueueItfVtable);
        ((*vt).enqueue)(bq_itf, bytes.as_ptr() as *const c_void, bytes.len() as u32)
    };
    assert_eq!(r, SL_RESULT_SUCCESS, "Enqueue must succeed (0 SL errors)");

    // The buffer-queue state reports the queued buffer.
    let mut bqs = SlBufferQueueState { count: 0, index: 0 };
    // SAFETY: bq_itf is the bq itf; bqs is a valid out-param.
    let r = unsafe {
        let vt = *(bq_itf as *const *const BufferQueueItfVtable);
        ((*vt).get_state)(bq_itf, &mut bqs)
    };
    assert_eq!(r, SL_RESULT_SUCCESS);
    assert!(
        bqs.count >= 1,
        "the enqueued buffer is queued (or already draining on a device)"
    );

    // GetInterface(SL_IID_RECORD) must fail — the player has no record itf (no fabricated success).
    let mut rec_itf: *mut c_void = std::ptr::null_mut();
    assert_eq!(
        obj_get_interface(player, iid_value(5), std::ptr::addr_of_mut!(rec_itf).cast()),
        SL_RESULT_PARAMETER_INVALID
    );

    // Clean up (drops the stream if any).
    obj_destroy(player);
    obj_destroy(mix);
    obj_destroy(engine);
}

#[test]
fn create_audio_player_rejects_non_pcm_source() {
    // A non-PCM (or non-buffer-queue) source must fail cleanly, never produce a player.
    let mut engine: *mut c_void = std::ptr::null_mut();
    // SAFETY: valid out-param.
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

    // A bad locator (not buffer-queue): use the OutputMix tag as the source locator.
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
    // SAFETY: eng_itf is the engine itf; src/snk valid.
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

/// The `SLInterfaceID` value the engine passes for `SL_IID[index]` (the value stored at the data
/// symbol). Mirrors `super::get_interface_for_test` but usable from the test module.
fn iid_value(index: usize) -> *const c_void {
    let addr = crate::loader::native_provider::sl_iid_addr_for_test(index);
    // SAFETY: `addr` is the data symbol's address; read the `SLInterfaceID` value stored there.
    unsafe { *(addr as *const *const c_void) }
}
