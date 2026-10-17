use std::f64::consts::PI;
use std::ffi::CString;
use std::fmt;
use std::io::{self, Read as _};
use std::ops::Range;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

#[cfg(test)]
use super::avcodec::Codec;

const MAX_INDEX_BYTES: u64 = 16 << 20;
const MAX_PACKET_BYTES: u64 = 256 << 20;
const SHA256_HEX_LEN: usize = 64;
const TONE_MARGIN_DB: f64 = 20.0;
const OFF_TONE_RATIOS: [f64; 4] = [0.5, 0.9, 1.1, 2.0];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    mime: String,
    width: Option<usize>,
    height: Option<usize>,
    sample_rate: Option<u32>,
    channel_count: Option<usize>,
    samples_per_frame: Option<usize>,
    tone_hz: Option<f64>,
    #[serde(rename = "csd-0")]
    csd_0: String,
    #[serde(rename = "csd-1")]
    csd_1: String,
    packets: Vec<IndexPacket>,
    frames: usize,
    frame_sha256: Option<Vec<String>>,
    stream_sha256: Option<String>,
    #[serde(default)]
    bench: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexPacket {
    offset: usize,
    size: usize,
    pts_us: i64,
    flags: u32,
}

pub(super) struct Packet {
    bytes: Range<usize>,
    pub(super) pts_us: i64,
    pub(super) flags: u32,
}

pub(super) enum VideoCheck {
    Hashes { frames: Vec<String>, stream: String },
    Timed,
}

pub(super) struct VideoTrack {
    pub(super) width: usize,
    pub(super) height: usize,
    pub(super) check: VideoCheck,
}

pub(super) struct AudioTrack {
    pub(super) sample_rate: u32,
    pub(super) channels: usize,
    pub(super) samples_per_frame: usize,
    pub(super) tone_hz: f64,
}

pub(super) enum Track {
    Video(VideoTrack),
    Audio(AudioTrack),
}

pub(super) struct Fixture {
    name: String,
    pub(super) mime: CString,
    pub(super) track: Track,
    csd_0: Vec<u8>,
    csd_1: Vec<u8>,
    pub(super) packets: Vec<Packet>,
    pub(super) frames: usize,
    data: Vec<u8>,
}

#[derive(Debug)]
pub(super) enum FixtureError {
    Read {
        path: PathBuf,
        source: io::Error,
    },
    TooLarge {
        path: PathBuf,
        limit: u64,
    },
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    Invalid {
        path: PathBuf,
        reason: String,
    },
    Empty {
        dir: PathBuf,
    },
}

impl fmt::Display for FixtureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => write!(f, "cannot read {}: {source}", path.display()),
            Self::TooLarge { path, limit } => {
                write!(f, "{} is larger than {limit} bytes", path.display())
            }
            Self::Parse { path, source } => {
                write!(
                    f,
                    "{} is not a media fixture index: {source}",
                    path.display()
                )
            }
            Self::Invalid { path, reason } => write!(f, "{}: {reason}", path.display()),
            Self::Empty { dir } => write!(f, "{} holds no media fixtures (*.json)", dir.display()),
        }
    }
}

impl std::error::Error for FixtureError {}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, FixtureError> {
    let read_error = |source| FixtureError::Read {
        path: path.to_owned(),
        source,
    };
    let file = std::fs::File::open(path).map_err(read_error)?;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(read_error)?;
    if bytes.len() as u64 > limit {
        return Err(FixtureError::TooLarge {
            path: path.to_owned(),
            limit,
        });
    }
    Ok(bytes)
}

fn hex_bytes(field: &str, text: &str) -> Result<Vec<u8>, String> {
    let (pairs, odd) = text.as_bytes().as_chunks::<2>();
    if !odd.is_empty() {
        return Err(format!("{field} has an odd number of hex digits"));
    }
    let digit = |byte: u8| char::from(byte).to_digit(16);
    pairs
        .iter()
        .map(|&[high, low]| {
            let byte = digit(high)? << 4 | digit(low)?;
            u8::try_from(byte).ok()
        })
        .collect::<Option<_>>()
        .ok_or_else(|| format!("{field} is not hex"))
}

fn sha256_text(field: &str, text: String) -> Result<String, String> {
    let valid = text.len() == SHA256_HEX_LEN
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if !valid {
        return Err(format!("{field} is not a lowercase SHA-256"));
    }
    Ok(text)
}

fn packets(index: Vec<IndexPacket>, data_len: usize) -> Result<Vec<Packet>, String> {
    if index.is_empty() {
        return Err("the index lists no packets".to_owned());
    }
    index
        .into_iter()
        .enumerate()
        .map(|(number, packet)| {
            let end = packet.offset.checked_add(packet.size).filter(|&end| {
                packet.size > 0 && end <= data_len
            });
            let Some(end) = end else {
                return Err(format!(
                    "packet {number} ({} bytes at {}) is empty or outside the {data_len}-byte packet file",
                    packet.size, packet.offset
                ));
            };
            Ok(Packet {
                bytes: packet.offset..end,
                pts_us: packet.pts_us,
                flags: packet.flags,
            })
        })
        .collect()
}

fn video_check(index: &mut Index) -> Result<VideoCheck, String> {
    match (
        index.bench,
        index.frame_sha256.take(),
        index.stream_sha256.take(),
    ) {
        (true, None, None) => Ok(VideoCheck::Timed),
        (false, Some(frames), Some(stream)) => {
            if frames.len() != index.frames {
                return Err(format!(
                    "frame_sha256 has {} hashes for {} frames",
                    frames.len(),
                    index.frames
                ));
            }
            let frames = frames
                .into_iter()
                .map(|hash| sha256_text("frame_sha256", hash))
                .collect::<Result<_, _>>()?;
            Ok(VideoCheck::Hashes {
                frames,
                stream: sha256_text("stream_sha256", stream)?,
            })
        }
        (true, _, _) => Err("a bench clip has no hashes".to_owned()),
        (false, _, _) => Err("a video fixture needs frame_sha256 and stream_sha256".to_owned()),
    }
}

fn track(index: &mut Index) -> Result<Track, String> {
    let video = (index.width, index.height);
    let audio = (
        index.sample_rate,
        index.channel_count,
        index.samples_per_frame,
        index.tone_hz,
    );
    match (video, audio) {
        ((Some(width @ 1..), Some(height @ 1..)), (None, None, None, None)) => {
            Ok(Track::Video(VideoTrack {
                width,
                height,
                check: video_check(index)?,
            }))
        }
        (
            (None, None),
            (
                Some(sample_rate @ 1..),
                Some(channels @ 1..),
                Some(samples_per_frame @ 1..),
                Some(tone_hz),
            ),
        ) => {
            if index.bench || index.frame_sha256.is_some() || index.stream_sha256.is_some() {
                return Err("an audio fixture has neither hashes nor a bench clip".to_owned());
            }
            if !(tone_hz > 0.0 && tone_hz < f64::from(sample_rate) / 2.0) {
                return Err(format!("tone_hz {tone_hz} is not below the Nyquist rate"));
            }
            Ok(Track::Audio(AudioTrack {
                sample_rate,
                channels,
                samples_per_frame,
                tone_hz,
            }))
        }
        _ => Err(
            "a fixture needs a positive width and height, or a positive sample_rate, \
             channel_count, samples_per_frame and tone_hz, but not both"
                .to_owned(),
        ),
    }
}

impl Fixture {
    pub(super) fn read(dir: &Path, name: &str) -> Result<Self, FixtureError> {
        let index_path = dir.join(format!("{name}.json"));
        let index = read_bounded(&index_path, MAX_INDEX_BYTES)?;
        let mut index: Index =
            serde_json::from_slice(&index).map_err(|source| FixtureError::Parse {
                path: index_path.clone(),
                source,
            })?;
        let data = read_bounded(&dir.join(format!("{name}.bin")), MAX_PACKET_BYTES)?;
        let invalid = |reason| FixtureError::Invalid {
            path: index_path.clone(),
            reason,
        };
        let mime = CString::new(index.mime.as_str())
            .ok()
            .filter(|mime| !mime.is_empty() && mime.to_bytes().iter().all(u8::is_ascii_graphic))
            .ok_or_else(|| invalid(format!("{:?} is not a MIME type", index.mime)))?;
        if index.frames == 0 {
            return Err(invalid("the index expects no frames".to_owned()));
        }
        let track = track(&mut index).map_err(invalid)?;
        let csd_0 = hex_bytes("csd-0", &index.csd_0).map_err(invalid)?;
        let csd_1 = hex_bytes("csd-1", &index.csd_1).map_err(invalid)?;
        let packets = packets(index.packets, data.len()).map_err(invalid)?;
        Ok(Self {
            name: name.to_owned(),
            mime,
            track,
            csd_0,
            csd_1,
            packets,
            frames: index.frames,
            data,
        })
    }

    pub(super) fn read_all(dir: &Path) -> Result<Vec<Self>, FixtureError> {
        let read_error = |source| FixtureError::Read {
            path: dir.to_owned(),
            source,
        };
        let mut names = Vec::new();
        for entry in std::fs::read_dir(dir).map_err(read_error)? {
            let path = entry.map_err(read_error)?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                if let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) {
                    names.push(name.to_owned());
                }
            }
        }
        if names.is_empty() {
            return Err(FixtureError::Empty {
                dir: dir.to_owned(),
            });
        }
        names.sort_unstable();
        names.iter().map(|name| Self::read(dir, name)).collect()
    }

    #[cfg(test)]
    pub(super) fn committed_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/media")
    }

    #[cfg(test)]
    pub(super) fn load(name: &str) -> Self {
        Self::read(&Self::committed_dir(), name)
            .unwrap_or_else(|error| panic!("load the {name} fixture: {error}"))
    }

    #[cfg(test)]
    pub(super) fn codec(&self) -> Codec {
        Codec::for_mime(self.mime.to_bytes())
            .unwrap_or_else(|| panic!("no decoder table row for {:?}", self.mime))
    }

    #[cfg(test)]
    pub(super) fn video(&self) -> &VideoTrack {
        match &self.track {
            Track::Video(video) => video,
            Track::Audio(_) => panic!("{} is an audio fixture", self.name),
        }
    }

    #[cfg(test)]
    pub(super) fn audio(&self) -> &AudioTrack {
        match &self.track {
            Track::Audio(audio) => audio,
            Track::Video(_) => panic!("{} is a video fixture", self.name),
        }
    }

    #[cfg(test)]
    pub(super) fn hashes(&self) -> (&[String], &str) {
        match &self.video().check {
            VideoCheck::Hashes { frames, stream } => (frames, stream),
            VideoCheck::Timed => panic!("{} is a bench clip without hashes", self.name),
        }
    }

    #[cfg(test)]
    pub(super) fn extradata(&self) -> Vec<u8> {
        let mut extradata = self.csd_0.clone();
        extradata.extend_from_slice(&self.csd_1);
        extradata
    }

    pub(super) fn name(&self) -> &str {
        &self.name
    }

    pub(super) fn csd_0(&self) -> &[u8] {
        &self.csd_0
    }

    pub(super) fn csd_1(&self) -> &[u8] {
        &self.csd_1
    }

    pub(super) fn packet(&self, index: usize) -> &[u8] {
        &self.data[self.packets[index].bytes.clone()]
    }
}

impl AudioTrack {
    pub(super) fn check_tone(&self, channels: &[Vec<f32>]) -> Result<(), String> {
        let rate = f64::from(self.sample_rate);
        for (channel, samples) in channels.iter().enumerate() {
            let tone = goertzel_power(samples, rate, self.tone_hz);
            for ratio in OFF_TONE_RATIOS {
                let other = self.tone_hz * ratio;
                let margin = 10.0 * (tone / goertzel_power(samples, rate, other)).log10();
                if margin.is_nan() || margin < TONE_MARGIN_DB {
                    return Err(format!(
                        "channel {channel}: {} Hz is only {margin:.1} dB above {other} Hz",
                        self.tone_hz
                    ));
                }
            }
        }
        Ok(())
    }
}

fn goertzel_power(samples: &[f32], sample_rate: f64, frequency: f64) -> f64 {
    let coefficient = 2.0 * (2.0 * PI * frequency / sample_rate).cos();
    let (mut previous, mut before_previous) = (0.0, 0.0);
    for &sample in samples {
        let current = f64::from(sample) + coefficient * previous - before_previous;
        before_previous = previous;
        previous = current;
    }
    previous * previous + before_previous * before_previous
        - coefficient * previous * before_previous
}

pub(super) fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub(super) fn hex(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
pub(super) mod tests;
