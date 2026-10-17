use std::sync::OnceLock;

use serde_json::Value;

use super::*;
use crate::loader::mediacodec::avcodec::Codec;
use crate::loader::mediacodec::codec::tests::decodable;
use crate::loader::mediacodec::fixture::tests::{copy_fixture, scratch};

fn missing_backend() -> &'static Backend {
    static MISSING: OnceLock<Backend> = OnceLock::new();
    MISSING.get_or_init(|| Backend::load(&[]))
}

fn report_lines(report: &MediaTestReport) -> Vec<String> {
    report.to_string().lines().map(str::to_owned).collect()
}

#[test]
fn committed_fixtures_decode_or_name_the_missing_decoder() {
    let report = run(&Fixture::committed_dir()).unwrap();
    let lines = report_lines(&report);
    assert!(report.passed(), "{lines:#?}");
    assert_eq!(lines.len(), 4, "{lines:#?}");
    assert!(lines[0].starts_with("backend: "), "{}", lines[0]);

    let expected = [
        (Codec::Aac, "audio/mp4a-latm: ok (aac-lc through buffers)"),
        (
            Codec::H264,
            "video/avc: ok (h264-bframes through buffers and an image reader)",
        ),
        (
            Codec::Hevc,
            "video/hevc: ok (hevc through buffers and an image reader)",
        ),
    ];
    for ((codec, ok), line) in expected.into_iter().zip(&lines[1..]) {
        if avcodec::backend().decoders().contains(&codec) {
            assert_eq!(line, ok);
        } else {
            let mime = codec.mime();
            assert!(line.starts_with(&format!("{mime}: no decoder: ")), "{line}");
        }
    }
}

#[test]
fn an_unavailable_backend_reports_no_decoder_for_every_mime_and_still_passes() {
    let report = run_with(&Fixture::committed_dir(), missing_backend).unwrap();
    let lines = report_lines(&report);
    assert!(report.passed(), "{lines:#?}");
    assert!(
        lines[0].starts_with("backend: unavailable: FFmpeg is not installed"),
        "{}",
        lines[0]
    );
    for (line, mime) in lines[1..]
        .iter()
        .zip(["audio/mp4a-latm", "video/avc", "video/hevc"])
    {
        assert!(
            line.starts_with(&format!("{mime}: no decoder: FFmpeg is not installed")),
            "{line}"
        );
    }
}

#[test]
fn a_mime_outside_the_decoder_table_has_no_decoder() {
    let dir = scratch("self-test-vp9");
    copy_fixture(&dir, "h264-bframes", |index| {
        index["mime"] = Value::from("video/x-vnd.on2.vp9");
    });
    let report = run(&dir).unwrap();
    std::fs::remove_dir_all(&dir).ok();

    assert!(report.passed());
    assert_eq!(
        report_lines(&report)[1],
        "video/x-vnd.on2.vp9: no decoder: Eclipse has no decoder for this MIME type"
    );
}

#[test]
fn a_wrong_frame_hash_fails_the_run_and_names_the_frame() {
    if !decodable(
        "a_wrong_frame_hash_fails_the_run_and_names_the_frame",
        Codec::H264,
    ) {
        return;
    }
    let dir = scratch("self-test-wrong");
    let wrong = "0".repeat(64);
    let mut expected = String::new();
    copy_fixture(&dir, "h264-bframes", |index| {
        expected = index["frame_sha256"][3].as_str().unwrap().to_owned();
        index["frame_sha256"][3] = Value::from(wrong.as_str());
    });
    let report = run(&dir).unwrap();
    std::fs::remove_dir_all(&dir).ok();

    assert!(!report.passed());
    assert_eq!(
        report_lines(&report)[1],
        format!(
            "video/avc: wrong (h264-bframes through buffers): frame 3 has the NV12 SHA-256 \
             {expected}, not {wrong}"
        )
    );
}

#[test]
fn a_wrong_tone_fails_the_audio_fixture() {
    if !decodable("a_wrong_tone_fails_the_audio_fixture", Codec::Aac) {
        return;
    }
    let dir = scratch("self-test-tone");
    copy_fixture(&dir, "aac-lc", |index| {
        index["tone_hz"] = Value::from(1000);
    });
    let report = run(&dir).unwrap();
    std::fs::remove_dir_all(&dir).ok();

    assert!(!report.passed());
    let line = &report_lines(&report)[1];
    assert!(
        line.starts_with(
            "audio/mp4a-latm: wrong (aac-lc through buffers): channel 0: 1000 Hz is only"
        ),
        "{line}"
    );
}

#[test]
fn a_bench_clip_reports_per_frame_decode_times() {
    if !decodable("a_bench_clip_reports_per_frame_decode_times", Codec::H264) {
        return;
    }
    let dir = scratch("self-test-bench");
    copy_fixture(&dir, "h264-bframes", |index| {
        let index = index.as_object_mut().unwrap();
        index.remove("frame_sha256");
        index.remove("stream_sha256");
        index.insert("bench".to_owned(), Value::from(true));
    });
    let report = run(&dir).unwrap();
    std::fs::remove_dir_all(&dir).ok();

    assert!(report.passed());
    let line = &report_lines(&report)[1];
    assert!(
        line.starts_with("video/avc: ok (h264-bframes through buffers); 10 frames in "),
        "{line}"
    );
    assert!(line.contains(" a second; per frame median "), "{line}");
}

#[test]
fn unreadable_fixtures_are_errors_not_reports() {
    let dir = scratch("self-test-broken");
    copy_fixture(&dir, "hevc", |index| {
        index["frames"] = Value::from(0);
    });
    let broken = run(&dir).err();
    std::fs::remove_dir_all(&dir).ok();

    let broken = broken.expect("a fixture that expects no frames is an error");
    assert!(
        broken.ends_with("hevc.json: the index expects no frames"),
        "{broken}"
    );
    let missing = run(&dir).err().expect("a missing directory is an error");
    assert!(missing.starts_with("cannot read "), "{missing}");
}

#[test]
fn frame_times_print_their_distribution() {
    let per_frame = [4, 2, 3, 10, 1].map(Duration::from_millis).to_vec();
    let times = FrameTimes {
        total: Duration::from_millis(20),
        per_frame,
    };
    assert_eq!(
        times.to_string(),
        "5 frames in 0.02 s, 250.0 a second; per frame median 3.00 ms, p90 10.00 ms, \
         p99 10.00 ms, max 10.00 ms"
    );
    let none = FrameTimes {
        total: Duration::ZERO,
        per_frame: Vec::new(),
    };
    assert_eq!(none.to_string(), "no frames timed");
}

#[test]
fn every_native_the_self_test_calls_is_the_one_the_provider_resolves() {
    assert_eq!(check_registration(), Ok(()));
}
