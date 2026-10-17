use std::collections::BTreeSet;

use sha2::{Digest, Sha256};

use super::*;
use crate::loader::log_capture::formatted_log;
use crate::loader::mediacodec::fixture::{hex, sha256_hex, Fixture};

const MISSING_PAIR: LibraryPair = LibraryPair {
    avcodec: "libavcodec.so.eclipse-missing",
    avcodec_major: 62,
    avutil: "libavutil.so.eclipse-missing",
    avutil_major: 60,
};

fn loaded_library(test: &str) -> Option<&'static Avcodec> {
    match backend().library() {
        Ok(library) => Some(library),
        Err(reason) => {
            eprintln!("{test}: {reason}; skipping");
            None
        }
    }
}

fn library_with(test: &str, codec: Codec) -> Option<&'static Avcodec> {
    let library = loaded_library(test)?;
    if !backend().decoders().contains(&codec) {
        eprintln!(
            "{test}: {} has no {codec:?} decoder; skipping",
            library.soname()
        );
        return None;
    }
    Some(library)
}

fn receive_ready<'lib>(session: &mut DecoderSession<'lib>, frames: &mut Vec<DecodedFrame<'lib>>) {
    loop {
        match session.receive().expect("receive a frame") {
            Received::Frame(frame) => frames.push(frame),
            Received::NeedsInput => return,
            Received::EndOfStream => panic!("the decoder ended before the stream did"),
        }
    }
}

fn send_packet<'lib>(
    session: &mut DecoderSession<'lib>,
    frames: &mut Vec<DecodedFrame<'lib>>,
    fixture: &Fixture,
    index: usize,
) {
    let packet = &fixture.packets[index];
    let bytes = fixture.packet(index);
    while session
        .send(bytes, packet.pts_us, packet.flags & 1 != 0)
        .expect("send a packet")
        == Sent::NeedsReceive
    {
        let before = frames.len();
        receive_ready(session, frames);
        assert!(frames.len() > before, "a full decoder must have output");
    }
    receive_ready(session, frames);
}

fn decode_all<'lib>(
    session: &mut DecoderSession<'lib>,
    fixture: &Fixture,
) -> Vec<DecodedFrame<'lib>> {
    let mut frames = Vec::new();
    for index in 0..fixture.packets.len() {
        send_packet(session, &mut frames, fixture, index);
    }
    while session.send_end_of_stream().expect("end the stream") == Sent::NeedsReceive {
        receive_ready(session, &mut frames);
    }
    loop {
        match session.receive().expect("drain the decoder") {
            Received::Frame(frame) => frames.push(frame),
            Received::EndOfStream => return frames,
            Received::NeedsInput => panic!("a draining decoder asked for input"),
        }
    }
}

fn video_frames<'a, 'lib>(frames: &'a [DecodedFrame<'lib>]) -> Vec<&'a VideoFrame<'lib>> {
    frames
        .iter()
        .map(|frame| match frame {
            DecodedFrame::Video(video) => video,
            DecodedFrame::Audio(_) => panic!("a video decoder produced audio"),
        })
        .collect()
}

fn audio_frames<'a, 'lib>(frames: &'a [DecodedFrame<'lib>]) -> Vec<&'a AudioFrame<'lib>> {
    frames
        .iter()
        .map(|frame| match frame {
            DecodedFrame::Audio(audio) => audio,
            DecodedFrame::Video(_) => panic!("an audio decoder produced video"),
        })
        .collect()
}

fn assert_matches_video_fixture(name: &str, fixture: &Fixture, frames: &[DecodedFrame<'_>]) {
    assert_eq!(frames.len(), fixture.frames, "{name} frame count");
    let pts: Vec<i64> = frames
        .iter()
        .map(|frame| frame.pts_us().expect("a decoded frame keeps its pts"))
        .collect();
    assert!(
        pts.windows(2).all(|pair| pair[0] < pair[1]),
        "{name} pts must ascend: {pts:?}"
    );
    let sent: BTreeSet<i64> = fixture.packets.iter().map(|packet| packet.pts_us).collect();
    assert_eq!(
        pts.iter().copied().collect::<BTreeSet<_>>(),
        sent,
        "{name} pts set"
    );

    let mut nv12 = Vec::new();
    let mut stream = Sha256::new();
    let (frame_hashes, stream_hash) = fixture.hashes();
    for (index, video) in video_frames(frames).into_iter().enumerate() {
        let format = video.format();
        assert_eq!(format.width, fixture.video().width, "{name} width");
        assert_eq!(format.height, fixture.video().height, "{name} height");
        video.write_nv12(&mut nv12);
        assert_eq!(nv12.len(), format.width * format.height * 3 / 2);
        assert_eq!(
            sha256_hex(&nv12),
            frame_hashes[index],
            "{name} frame {index} NV12 hash"
        );
        stream.update(&nv12);
    }
    assert_eq!(
        hex(&stream.finalize()),
        stream_hash,
        "{name} stream NV12 hash"
    );
}

fn check_video_fixture(test: &str, name: &str) {
    let fixture = Fixture::load(name);
    let codec = fixture.codec();
    let Some(library) = library_with(test, codec) else {
        return;
    };
    let mut session =
        DecoderSession::open(library, codec, &fixture.extradata()).expect("open the decoder");
    let frames = decode_all(&mut session, &fixture);
    assert_matches_video_fixture(name, &fixture, &frames);
}

#[test]
fn ffmpeg_error_codes_match_libavutil() {
    assert_eq!(AVERROR_EAGAIN, -11);
    assert_eq!(AVERROR_EOF, -541_478_725);
    assert_eq!(AVERROR_INVALIDDATA, -1_094_995_529);
}

#[test]
fn missing_libraries_report_not_found_and_no_decoders() {
    let backend = Backend::load(&[MISSING_PAIR]);
    match backend.library() {
        Err(BackendUnavailable::NotFound { tried }) => {
            assert_eq!(tried, &vec![MISSING_PAIR.avcodec]);
        }
        Err(other) => panic!("expected NotFound, got {other:?}"),
        Ok(library) => panic!("{} loaded from a missing soname", library.soname()),
    }
    assert!(backend.decoders().is_empty());
}

#[test]
fn an_unavailable_backend_says_why_at_info() {
    let backend = Backend::load(&[MISSING_PAIR]);
    let log = formatted_log("mediandk=info", || backend.report());
    let line = log
        .lines()
        .find(|line| line.contains("INFO mediandk:"))
        .unwrap_or_else(|| panic!("no info line in {log:?}"));
    assert!(
        line.contains("tried libavcodec.so.eclipse-missing"),
        "{line}"
    );
    assert!(line.contains("install FFmpeg 8 or 9"), "{line}");
    assert!(
        line.contains("H.264, HEVC and AAC media will not play"),
        "{line}"
    );
}

#[test]
fn the_decoder_table_names_each_mime_and_decoder_once() {
    let rows: Vec<(&str, &str)> = Codec::ALL
        .into_iter()
        .map(|codec| (codec.mime(), codec.decoder_name().to_str().unwrap()))
        .collect();
    assert_eq!(
        rows,
        [
            ("audio/mp4a-latm", "aac"),
            ("video/avc", "h264"),
            ("video/hevc", "hevc"),
            ("audio/mpeg", "mp3"),
            ("audio/opus", "opus"),
        ]
    );
}

#[test]
fn listing_decoders_answers_what_the_codec_backend_offers() {
    assert_eq!(available_decoders(), backend().decoders());
    assert_eq!(available_decoders(), backend().decoders());
}

#[test]
fn library_versions_print_major_minor_micro() {
    let version = LibraryVersion((62 << 16) | (28 << 8) | 102);
    assert_eq!(version.major(), 62);
    assert_eq!(version.to_string(), "62.28.102");
}

#[test]
fn float_samples_round_and_saturate_to_i16() {
    assert_eq!(float_to_i16(0.0), 0);
    assert_eq!(float_to_i16(0.5), 16_384);
    assert_eq!(float_to_i16(-1.0), i16::MIN);
    assert_eq!(float_to_i16(1.0), i16::MAX);
    assert_eq!(float_to_i16(4.0), i16::MAX);
    assert_eq!(float_to_i16(-4.0), i16::MIN);
    assert_eq!(float_to_i16(f32::NAN), 0);
}

#[test]
fn the_runtime_library_is_accepted_only_at_its_own_major() {
    let Some(library) = loaded_library("the_runtime_library_is_accepted_only_at_its_own_major")
    else {
        return;
    };
    let pair = RUNTIME_LIBRARIES
        .into_iter()
        .find(|pair| pair.avcodec == library.soname())
        .expect("the loaded library is a runtime pair");
    assert_eq!(library.version().major(), pair.avcodec_major);

    let wrong = LibraryPair {
        avcodec_major: pair.avcodec_major + 1,
        ..pair
    };
    match Backend::load(&[wrong]).library() {
        Err(BackendUnavailable::VersionMismatch {
            soname,
            expected,
            found,
        }) => {
            assert_eq!(*soname, pair.avcodec);
            assert_eq!(*expected, wrong.avcodec_major);
            assert_eq!(*found, library.version());
        }
        Err(other) => panic!("expected VersionMismatch, got {other:?}"),
        Ok(_) => panic!("a major-version mismatch was accepted"),
    }
}

#[test]
fn h264_with_b_frames_decodes_every_frame_in_presentation_order() {
    check_video_fixture(
        "h264_with_b_frames_decodes_every_frame_in_presentation_order",
        "h264-bframes",
    );
}

#[test]
fn hevc_decodes_every_frame_in_presentation_order() {
    check_video_fixture("hevc_decodes_every_frame_in_presentation_order", "hevc");
}

#[test]
fn aac_lc_decodes_to_a_stereo_440_hz_tone_at_48_khz() {
    let fixture = Fixture::load("aac-lc");
    let Some(library) = library_with(
        "aac_lc_decodes_to_a_stereo_440_hz_tone_at_48_khz",
        Codec::Aac,
    ) else {
        return;
    };
    let mut session =
        DecoderSession::open(library, Codec::Aac, &fixture.extradata()).expect("open the decoder");
    let frames = decode_all(&mut session, &fixture);
    assert_eq!(frames.len(), fixture.frames);

    let track = fixture.audio();
    assert_eq!(
        (track.sample_rate, track.channels, track.tone_hz),
        (48_000, 2, 440.0)
    );
    let expected = AudioFormat {
        sample_rate: track.sample_rate,
        channels: track.channels,
    };
    let mut channels = vec![Vec::new(); expected.channels];
    let mut pcm = Vec::new();
    for audio in audio_frames(&frames) {
        assert_eq!(audio.format(), expected);
        audio.write_pcm(PcmEncoding::F32, &mut pcm);
        assert_eq!(
            pcm.len(),
            track.samples_per_frame * expected.channels * size_of::<f32>()
        );
        let values = pcm
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| f32::from_ne_bytes(*bytes));
        for (index, value) in values.enumerate() {
            channels[index % expected.channels].push(value);
        }
    }
    for samples in &channels {
        assert_eq!(
            samples.len(),
            fixture.packets.len() * track.samples_per_frame
        );
    }
    track.check_tone(&channels).unwrap();
}

#[test]
fn audio_converts_to_matching_i16_and_f32_pcm() {
    let fixture = Fixture::load("aac-lc");
    let Some(library) = library_with("audio_converts_to_matching_i16_and_f32_pcm", Codec::Aac)
    else {
        return;
    };
    let mut session =
        DecoderSession::open(library, Codec::Aac, &fixture.extradata()).expect("open the decoder");
    let frames = decode_all(&mut session, &fixture);
    let samples_per_frame = fixture.audio().samples_per_frame;
    let (mut floats, mut shorts) = (Vec::new(), Vec::new());
    for audio in audio_frames(&frames) {
        let values = samples_per_frame * audio.format().channels;
        audio.write_pcm(PcmEncoding::F32, &mut floats);
        audio.write_pcm(PcmEncoding::I16, &mut shorts);
        assert_eq!(floats.len(), values * size_of::<f32>());
        assert_eq!(shorts.len(), values * size_of::<i16>());
        let floats = floats
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| f32::from_ne_bytes(*bytes));
        let shorts = shorts
            .as_chunks::<2>()
            .0
            .iter()
            .map(|bytes| i16::from_ne_bytes(*bytes));
        for (float, short) in floats.zip(shorts) {
            assert_eq!(short, float_to_i16(float));
        }
    }
}

#[test]
fn ten_bit_video_and_32_bit_audio_are_unsupported_formats() {
    let Some(library) = loaded_library("ten_bit_video_and_32_bit_audio_are_unsupported_formats")
    else {
        return;
    };
    let ten_bit = unsafe { (library.functions.av_get_pix_fmt)(c"yuv420p10le".as_ptr()) };
    assert_eq!(
        library.pixel_format(ten_bit),
        Err(DecodeError::UnsupportedPixelFormat(
            "yuv420p10le".to_owned()
        ))
    );
    let s32 = unsafe { (library.functions.av_get_sample_fmt)(c"s32".as_ptr()) };
    assert_eq!(
        library.sample_format(s32),
        Err(DecodeError::UnsupportedSampleFormat("s32".to_owned()))
    );
    let yuvj = unsafe { (library.functions.av_get_pix_fmt)(c"yuvj420p".as_ptr()) };
    assert_eq!(library.pixel_format(yuvj), Ok(PixelFormat::Yuvj420p));
}

#[test]
fn an_empty_packet_neither_decodes_nor_ends_the_stream() {
    let fixture = Fixture::load("h264-bframes");
    let Some(library) = library_with(
        "an_empty_packet_neither_decodes_nor_ends_the_stream",
        Codec::H264,
    ) else {
        return;
    };
    let mut session =
        DecoderSession::open(library, Codec::H264, &fixture.extradata()).expect("open the decoder");
    assert_eq!(session.send(&[], 0, false), Ok(Sent::Accepted));
    assert!(matches!(session.receive(), Ok(Received::NeedsInput)));
    let frames = decode_all(&mut session, &fixture);
    assert_matches_video_fixture("h264-bframes", &fixture, &frames);
}

#[test]
fn flush_drops_pending_frames_and_restarts_at_a_keyframe() {
    let fixture = Fixture::load("h264-bframes");
    let Some(library) = library_with(
        "flush_drops_pending_frames_and_restarts_at_a_keyframe",
        Codec::H264,
    ) else {
        return;
    };
    let mut session =
        DecoderSession::open(library, Codec::H264, &fixture.extradata()).expect("open the decoder");
    let mut early = Vec::new();
    for index in 0..4 {
        send_packet(&mut session, &mut early, &fixture, index);
    }
    drop(early);
    session.flush();
    assert!(matches!(session.receive(), Ok(Received::NeedsInput)));
    let frames = decode_all(&mut session, &fixture);
    assert_matches_video_fixture("h264-bframes", &fixture, &frames);

    session.flush();
    let again = decode_all(&mut session, &fixture);
    assert_matches_video_fixture("h264-bframes after end of stream", &fixture, &again);
}

#[test]
fn odd_sized_pictures_pack_to_an_even_nv12_stride_and_slice_height() {
    let Some(library) =
        loaded_library("odd_sized_pictures_pack_to_an_even_nv12_stride_and_slice_height")
    else {
        return;
    };
    let functions = &library.functions;
    let frame = FfmpegBox::new(
        unsafe { (functions.av_frame_alloc)() },
        functions.av_frame_free,
        "a frame",
    )
    .expect("allocate a frame");
    let mut luma = [1, 2, 3, 0, 4, 5, 6, 0, 7, 8, 9, 0];
    let mut u = [10, 11, 12, 13];
    let mut v = [20, 21, 22, 23];
    let yuv420p = unsafe { (functions.av_get_pix_fmt)(c"yuv420p".as_ptr()) };
    unsafe {
        let raw = frame.as_ptr();
        (*raw).data[0] = luma.as_mut_ptr();
        (*raw).linesize[0] = 4;
        (*raw).data[1] = u.as_mut_ptr();
        (*raw).linesize[1] = 2;
        (*raw).data[2] = v.as_mut_ptr();
        (*raw).linesize[2] = 2;
        (*raw).width = 3;
        (*raw).height = 3;
        (*raw).format = yuv420p;
    }
    let video = VideoFrame::new(library, frame).expect("a 3x3 yuv420p picture");
    let mut nv12 = Vec::new();
    video.write_nv12(&mut nv12);
    assert_eq!(
        Nv12Layout::new(3, 3),
        Nv12Layout {
            stride: 4,
            slice_height: 4
        }
    );
    assert_eq!(
        nv12,
        [1, 2, 3, 0, 4, 5, 6, 0, 7, 8, 9, 0, 0, 0, 0, 0, 10, 20, 11, 21, 12, 22, 13, 23]
    );
}
