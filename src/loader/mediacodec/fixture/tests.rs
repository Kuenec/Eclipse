use std::path::{Path, PathBuf};

use serde_json::Value;

use super::*;

pub(in crate::loader::mediacodec) fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("eclipse-media-{tag}-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub(in crate::loader::mediacodec) fn copy_fixture(
    dir: &Path,
    name: &str,
    edit_index: impl FnOnce(&mut Value),
) {
    let committed = Fixture::committed_dir();
    let mut index: Value =
        serde_json::from_slice(&std::fs::read(committed.join(format!("{name}.json"))).unwrap())
            .unwrap();
    edit_index(&mut index);
    std::fs::write(
        dir.join(format!("{name}.json")),
        serde_json::to_vec_pretty(&index).unwrap(),
    )
    .unwrap();
    std::fs::copy(
        committed.join(format!("{name}.bin")),
        dir.join(format!("{name}.bin")),
    )
    .unwrap();
}

fn invalid_reason(dir: &Path, name: &str) -> String {
    match Fixture::read(dir, name) {
        Err(FixtureError::Invalid { path, reason }) => {
            assert_eq!(path, dir.join(format!("{name}.json")));
            reason
        }
        Err(error) => panic!("{name}: expected an invalid index, got {error}"),
        Ok(_) => panic!("{name}: the index was accepted"),
    }
}

fn sine(frequency: f64, sample_rate: u32, samples: usize) -> Vec<f32> {
    (0..samples)
        .map(|index| {
            let time = index as f64 / f64::from(sample_rate);
            (0.5 * (2.0 * PI * frequency * time).sin()) as f32
        })
        .collect()
}

#[test]
fn committed_fixtures_parse_into_typed_tracks() {
    let fixtures = Fixture::read_all(&Fixture::committed_dir()).unwrap();
    let names: Vec<&str> = fixtures.iter().map(Fixture::name).collect();
    assert_eq!(names, ["aac-lc", "h264-bframes", "hevc"]);

    let Track::Audio(audio) = &fixtures[0].track else {
        panic!("aac-lc is audio");
    };
    assert_eq!(
        (audio.sample_rate, audio.channels, audio.samples_per_frame),
        (48_000, 2, 1024)
    );
    assert_eq!(audio.tone_hz, 440.0);
    assert_eq!(fixtures[0].csd_0(), [0x11, 0x90, 0x56, 0xe5, 0x00]);

    for fixture in &fixtures[1..] {
        let Track::Video(video) = &fixture.track else {
            panic!("{} is video", fixture.name());
        };
        assert_eq!((video.width, video.height), (64, 48));
        let VideoCheck::Hashes { frames, .. } = &video.check else {
            panic!("{} has hashes", fixture.name());
        };
        assert_eq!(frames.len(), fixture.frames);
        assert_eq!(fixture.packets.len(), 10);
        assert!(!fixture.packet(9).is_empty());
    }
}

#[test]
fn packets_outside_the_packet_file_are_rejected() {
    let dir = scratch("past-the-end");
    copy_fixture(&dir, "h264-bframes", |index| {
        index["packets"][9]["size"] = Value::from(100_000);
    });
    let reason = invalid_reason(&dir, "h264-bframes");
    copy_fixture(&dir, "hevc", |index| {
        index["packets"][0]["size"] = Value::from(0);
    });
    let empty = invalid_reason(&dir, "hevc");
    std::fs::remove_dir_all(&dir).ok();

    assert!(
        reason.starts_with("packet 9 (100000 bytes at 1359)"),
        "{reason}"
    );
    assert!(empty.starts_with("packet 0 (0 bytes at 0)"), "{empty}");
}

#[test]
fn video_hashes_must_cover_every_frame_as_lowercase_sha256() {
    let dir = scratch("hashes");
    copy_fixture(&dir, "h264-bframes", |index| {
        index["frame_sha256"].as_array_mut().unwrap().pop();
    });
    let short = invalid_reason(&dir, "h264-bframes");
    copy_fixture(&dir, "hevc", |index| {
        index["stream_sha256"] = Value::from("ABC");
    });
    let malformed = invalid_reason(&dir, "hevc");
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(short, "frame_sha256 has 9 hashes for 10 frames");
    assert_eq!(malformed, "stream_sha256 is not a lowercase SHA-256");
}

#[test]
fn a_bench_clip_is_timed_and_carries_no_hashes() {
    let dir = scratch("bench");
    copy_fixture(&dir, "h264-bframes", |index| {
        let index = index.as_object_mut().unwrap();
        index.remove("frame_sha256");
        index.remove("stream_sha256");
        index.insert("bench".to_owned(), Value::from(true));
    });
    let bench = Fixture::read(&dir, "h264-bframes").unwrap();
    copy_fixture(&dir, "hevc", |index| {
        index["bench"] = Value::from(true);
    });
    let hashed_bench = invalid_reason(&dir, "hevc");
    std::fs::remove_dir_all(&dir).ok();

    assert!(matches!(
        bench.track,
        Track::Video(VideoTrack {
            check: VideoCheck::Timed,
            ..
        })
    ));
    assert_eq!(hashed_bench, "a bench clip has no hashes");
}

#[test]
fn mixed_tracks_bad_hex_and_unknown_keys_are_rejected() {
    let dir = scratch("mixed");
    copy_fixture(&dir, "aac-lc", |index| {
        index["width"] = Value::from(64);
    });
    let mixed = invalid_reason(&dir, "aac-lc");
    copy_fixture(&dir, "hevc", |index| {
        index["csd-0"] = Value::from("0g");
    });
    let bad_hex = invalid_reason(&dir, "hevc");
    copy_fixture(&dir, "h264-bframes", |index| {
        index["colour"] = Value::from("blue");
    });
    let unknown = Fixture::read(&dir, "h264-bframes");
    std::fs::remove_dir_all(&dir).ok();

    assert!(
        mixed.starts_with("a fixture needs a positive width"),
        "{mixed}"
    );
    assert_eq!(bad_hex, "csd-0 is not hex");
    let Err(FixtureError::Parse { source, .. }) = unknown else {
        panic!("an unknown key is a parse error");
    };
    assert!(source.to_string().contains("colour"), "{source}");
}

#[test]
fn oversized_files_and_empty_directories_are_errors() {
    let dir = scratch("bounds");
    let file = dir.join("five.bin");
    std::fs::write(&file, b"12345").unwrap();
    let too_large = read_bounded(&file, 4);
    let fits = read_bounded(&file, 5);
    std::fs::remove_file(&file).unwrap();
    let empty = Fixture::read_all(&dir);
    std::fs::remove_dir_all(&dir).ok();

    assert!(matches!(
        too_large,
        Err(FixtureError::TooLarge { limit: 4, .. })
    ));
    assert_eq!(fits.unwrap(), b"12345");
    let Err(error) = empty else {
        panic!("a directory without indexes has no fixtures");
    };
    assert!(matches!(error, FixtureError::Empty { .. }));
    assert!(error
        .to_string()
        .ends_with("holds no media fixtures (*.json)"));
}

#[test]
fn the_tone_check_accepts_the_tone_and_rejects_a_shifted_pitch() {
    let track = AudioTrack {
        sample_rate: 48_000,
        channels: 2,
        samples_per_frame: 1024,
        tone_hz: 440.0,
    };
    let tone = sine(440.0, 48_000, 10_240);
    assert_eq!(track.check_tone(&[tone.clone(), tone]), Ok(()));

    let resampled = sine(440.0 * 48_000.0 / 44_100.0, 48_000, 10_240);
    let error = track.check_tone(&[resampled]).unwrap_err();
    assert!(error.starts_with("channel 0: 440 Hz is only"), "{error}");

    let silence = vec![0.0; 10_240];
    assert!(track.check_tone(&[silence]).is_err());
}

#[test]
fn hex_fields_take_only_pairs_of_hex_digits() {
    assert_eq!(hex_bytes("csd-0", "00aBff"), Ok(vec![0x00, 0xab, 0xff]));
    assert_eq!(hex_bytes("csd-1", ""), Ok(Vec::new()));
    for bad in ["+f", "0g", "-1", " 1"] {
        assert_eq!(
            hex_bytes("csd-0", bad),
            Err("csd-0 is not hex".to_owned()),
            "{bad}"
        );
    }
    assert_eq!(
        hex_bytes("csd-0", "abc"),
        Err("csd-0 has an odd number of hex digits".to_owned())
    );
}
