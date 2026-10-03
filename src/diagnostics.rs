use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::UNIX_EPOCH;

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::{FormatTime, SystemTime};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::Registry;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

const LOG_DIR: &str = "logs";
const LATEST_LOG: &str = "eclipse.log";
const LATEST_LOG_STAGING: &str = "eclipse.log.next";
const LEGACY_PREVIOUS_LOG: &str = "eclipse.log.1";
const RUN_PREFIX: &str = "eclipse-";
const HEAD_SUFFIX: &str = ".log";
const TAIL_SUFFIX: &str = ".tail.log";
const PREVIOUS_TAIL_SUFFIX: &str = ".tail.log.1";
const RUN_STAMP_SHAPE: &str = "00000000T000000.000Z";
const RECORD_STAMP_SHAPE: &str = "0000-00-00T00:00:00.000000Z";
pub const RAW_LINE_BYTES: usize = 64 * 1024;
const MIB: u64 = 1024 * 1024;
const RUN_LOG_LIMITS: RunLogLimits = RunLogLimits {
    head: 2 * MIB,
    tail_half: 5 * MIB,
    runs: 5,
    total: 64 * MIB,
};
pub const STATUS_TARGET: &str = "eclipse::status";
const SECONDS_PER_DAY: u64 = 86_400;
const DAYS_FROM_MARCH_OF_YEAR_ZERO_TO_EPOCH: u64 = 719_468;
const DAYS_PER_400_YEARS: u64 = 146_097;

static SUPERVISOR_PIPE: OnceLock<Mutex<File>> = OnceLock::new();

pub enum LogSink {
    Stderr,
    Supervisor(File),
}

struct RecordWriter;

struct EventFieldVisitor<'a>(&'a mut String);

impl Visit for EventFieldVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }
}

impl<S: Subscriber> Layer<S> for RecordWriter {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let line = event_line(event);
        match SUPERVISOR_PIPE.get() {
            Some(pipe) => send_to_supervisor(pipe, &line),
            None => {
                let _ = io::stderr().write_all(line.as_bytes());
            }
        }
    }
}

fn event_line(event: &Event<'_>) -> String {
    let meta = event.metadata();
    log_line(meta.level(), meta.target(), |line| {
        event.record(&mut EventFieldVisitor(line));
    })
}

#[cfg(test)]
pub(crate) fn captured_log_lines(body: impl FnOnce()) -> String {
    use std::sync::Arc;

    struct Capture(Arc<Mutex<String>>);

    impl<S: Subscriber> Layer<S> for Capture {
        fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push_str(&event_line(event));
        }
    }

    let lines = Arc::new(Mutex::new(String::new()));
    let subscriber = Registry::default().with(Capture(Arc::clone(&lines)));
    tracing::subscriber::with_default(subscriber, body);
    let captured = lines.lock().unwrap_or_else(PoisonError::into_inner);
    captured.clone()
}

fn log_line(level: &Level, target: &str, message: impl FnOnce(&mut String)) -> String {
    use std::fmt::Write as _;
    let mut line = String::with_capacity(256);
    let _ = SystemTime.format_time(&mut Writer::new(&mut line));
    let _ = write!(line, " {level:>5} {target}: ");
    message(&mut line);
    line.push('\n');
    line
}

fn send_to_supervisor(pipe: &Mutex<File>, line: &str) {
    let pipe = pipe.lock().unwrap_or_else(PoisonError::into_inner);
    let _ = (&*pipe).write_all(line.as_bytes());
}

pub fn init(sink: LogSink) {
    if let LogSink::Supervisor(pipe) = sink {
        let _ = SUPERVISOR_PIPE.set(Mutex::new(pipe));
    }
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();

    let _ = Registry::default()
        .with(filter)
        .with(RecordWriter)
        .try_init();
}

pub fn record_status(level: Level, text: &str) {
    if let Some(pipe) = SUPERVISOR_PIPE.get() {
        send_to_supervisor(
            pipe,
            &log_line(&level, STATUS_TARGET, |line| line.push_str(text)),
        );
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct RecordHead<'a> {
    pub level: Level,
    pub target: &'a str,
    pub message: &'a str,
}

pub fn record_head(line: &str) -> Option<RecordHead<'_>> {
    let (stamp, rest) = line.split_at_checked(RECORD_STAMP_SHAPE.len())?;
    if !matches_shape(stamp, RECORD_STAMP_SHAPE) {
        return None;
    }
    let (level, rest) = rest.strip_prefix(' ')?.trim_start().split_once(' ')?;
    let (target, message) = rest.split_once(": ")?;
    Some(RecordHead {
        level: level.parse().ok()?,
        target,
        message,
    })
}

fn matches_shape(text: &str, shape: &str) -> bool {
    text.len() == shape.len()
        && text
            .bytes()
            .zip(shape.bytes())
            .all(|(byte, shape)| match shape {
                b'0' => byte.is_ascii_digit(),
                _ => byte == shape,
            })
}

#[derive(Clone, Copy, Debug)]
pub enum RawStream {
    Stdout,
    Stderr,
}

impl RawStream {
    fn target(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

#[derive(Clone, Copy)]
struct RunLogLimits {
    head: u64,
    tail_half: u64,
    runs: usize,
    total: u64,
}

impl RunLogLimits {
    fn run_cap(self) -> u64 {
        self.head + 2 * self.tail_half
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RunStamp(String);

impl RunStamp {
    fn at(time: std::time::SystemTime) -> Self {
        let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or_default();
        let seconds = since_epoch.as_secs();
        let second_of_day = seconds % SECONDS_PER_DAY;
        let (year, month, day) = civil_from_days(seconds / SECONDS_PER_DAY);
        Self(format!(
            "{year:04}{month:02}{day:02}T{:02}{:02}{:02}.{:03}Z",
            second_of_day / 3_600,
            second_of_day / 60 % 60,
            second_of_day % 60,
            since_epoch.subsec_millis()
        ))
    }

    fn parse(text: &str) -> Option<Self> {
        matches_shape(text, RUN_STAMP_SHAPE).then(|| Self(text.to_owned()))
    }

    fn file_name(&self, suffix: &str) -> String {
        format!("{RUN_PREFIX}{}{suffix}", self.0)
    }
}

fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let shifted = days + DAYS_FROM_MARCH_OF_YEAR_ZERO_TO_EPOCH;
    let era = shifted / DAYS_PER_400_YEARS;
    let day_of_era = shifted % DAYS_PER_400_YEARS;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

fn run_stamp_of(file_name: &str) -> Option<RunStamp> {
    let (stamp, suffix) = file_name
        .strip_prefix(RUN_PREFIX)?
        .split_at_checked(RUN_STAMP_SHAPE.len())?;
    if !suffix.starts_with('.') {
        return None;
    }
    RunStamp::parse(stamp)
}

#[derive(Default)]
struct RunFiles {
    paths: Vec<PathBuf>,
    bytes: u64,
}

fn run_logs(dir: &Path) -> io::Result<Vec<RunFiles>> {
    let mut runs = BTreeMap::<RunStamp, RunFiles>::new();
    for entry in fs::read_dir(dir).map_err(failed("list", dir))? {
        let entry = entry.map_err(failed("list", dir))?;
        let Some(stamp) = entry.file_name().to_str().and_then(run_stamp_of) else {
            continue;
        };
        let metadata = entry.metadata().map_err(failed("inspect", &entry.path()))?;
        if !metadata.is_file() {
            continue;
        }
        let run = runs.entry(stamp).or_default();
        run.paths.push(entry.path());
        run.bytes += metadata.len();
    }
    Ok(runs.into_values().rev().collect())
}

pub fn log_dir(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join(LOG_DIR)
}

pub fn older_run_logs(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let runs = match run_logs(dir) {
        Ok(runs) => runs,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    Ok(runs.into_iter().skip(1).flat_map(|run| run.paths).collect())
}

fn open_run(
    dir: &Path,
    started: std::time::SystemTime,
    limits: RunLogLimits,
) -> io::Result<RunLog> {
    remove_if_present(&dir.join(LEGACY_PREVIOUS_LOG))?;
    adopt_legacy_log(dir)?;
    prune_runs(dir, limits)?;
    let log = RunLog::create(dir, RunStamp::at(started), limits)?;
    link_latest(dir, &log.stamp)?;
    Ok(log)
}

fn adopt_legacy_log(dir: &Path) -> io::Result<()> {
    let latest = dir.join(LATEST_LOG);
    let metadata = match fs::symlink_metadata(&latest) {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(failed("inspect", &latest)(error)),
    };
    let modified = metadata
        .modified()
        .map_err(failed("read the modification time of", &latest))?;
    let run = dir.join(RunStamp::at(modified).file_name(HEAD_SUFFIX));
    fs::rename(&latest, &run).map_err(failed("rename", &latest))
}

fn prune_runs(dir: &Path, limits: RunLogLimits) -> io::Result<()> {
    let budget = limits.total.saturating_sub(limits.run_cap());
    let mut kept_bytes = 0;
    let mut keeping = true;
    for (index, run) in run_logs(dir)?.into_iter().enumerate() {
        keeping = keeping && index + 1 < limits.runs && kept_bytes + run.bytes <= budget;
        if keeping {
            kept_bytes += run.bytes;
        } else {
            run.paths
                .iter()
                .try_for_each(|path| remove_if_present(path))?;
        }
    }
    Ok(())
}

fn link_latest(dir: &Path, stamp: &RunStamp) -> io::Result<()> {
    let staging = dir.join(LATEST_LOG_STAGING);
    remove_if_present(&staging)?;
    std::os::unix::fs::symlink(stamp.file_name(HEAD_SUFFIX), &staging)
        .map_err(failed("create the link", &staging))?;
    let latest = dir.join(LATEST_LOG);
    fs::rename(&staging, &latest).map_err(failed("replace", &latest))
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(failed("remove", path)(error)),
        _ => Ok(()),
    }
}

fn failed<'a>(operation: &'a str, path: &'a Path) -> impl FnOnce(io::Error) -> io::Error + 'a {
    move |error| {
        io::Error::new(
            error.kind(),
            format!("cannot {operation} {}: {error}", path.display()),
        )
    }
}

enum RunPart {
    Head,
    Tail,
}

pub struct RunLog {
    dir: PathBuf,
    stamp: RunStamp,
    file: BufWriter<File>,
    part: RunPart,
    written: u64,
    continuation_bytes: u64,
    limits: RunLogLimits,
}

impl RunLog {
    pub fn start(app_data_dir: &Path) -> io::Result<Self> {
        let dir = log_dir(app_data_dir);
        fs::create_dir_all(&dir).map_err(failed("create", &dir))?;
        open_run(&dir, std::time::SystemTime::now(), RUN_LOG_LIMITS)
    }

    fn create(dir: &Path, stamp: RunStamp, limits: RunLogLimits) -> io::Result<Self> {
        let head = dir.join(stamp.file_name(HEAD_SUFFIX));
        let file = File::create_new(&head).map_err(failed("create", &head))?;
        let continuation_bytes = continuation_line(&stamp).len() as u64;
        Ok(Self {
            dir: dir.to_owned(),
            stamp,
            file: BufWriter::new(file),
            part: RunPart::Head,
            written: 0,
            continuation_bytes,
            limits,
        })
    }

    pub fn head_path(&self) -> PathBuf {
        self.path(HEAD_SUFFIX)
    }

    fn path(&self, suffix: &str) -> PathBuf {
        self.dir.join(self.stamp.file_name(suffix))
    }

    pub fn append_line(&mut self, line: &[u8]) -> io::Result<()> {
        let length = line.len() as u64 + 1;
        self.make_room(length)?;
        self.file.write_all(line)?;
        self.file.write_all(b"\n")?;
        self.written += length;
        Ok(())
    }

    fn append(&mut self, record: &[u8]) -> io::Result<()> {
        let length = record.len() as u64;
        self.make_room(length)?;
        self.file.write_all(record)?;
        self.written += length;
        Ok(())
    }

    pub fn append_raw(&mut self, stream: RawStream, line: &[u8]) -> io::Result<()> {
        let text = String::from_utf8_lossy(line).replace('\0', "");
        let text = crate::links::redact_join_secrets(&text);
        let mut rest: &str = &text;
        loop {
            let (chunk, after) = rest.split_at(rest.floor_char_boundary(RAW_LINE_BYTES));
            let record = log_line(&Level::INFO, stream.target(), |line| line.push_str(chunk));
            self.append(record.as_bytes())?;
            if after.is_empty() {
                return Ok(());
            }
            rest = after;
        }
    }

    pub fn record(&mut self, level: Level, target: &str, text: &str) -> io::Result<()> {
        self.append(log_line(&level, target, |line| line.push_str(text)).as_bytes())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }

    fn make_room(&mut self, length: u64) -> io::Result<()> {
        if self.written == 0 {
            return Ok(());
        }
        match self.part {
            RunPart::Head if self.written + length + self.continuation_bytes > self.limits.head => {
                self.continue_in_tail()
            }
            RunPart::Tail if self.written + length > self.limits.tail_half => self.rotate_tail(),
            RunPart::Head | RunPart::Tail => Ok(()),
        }
    }

    fn continue_in_tail(&mut self) -> io::Result<()> {
        self.file
            .write_all(continuation_line(&self.stamp).as_bytes())?;
        self.file.flush()?;
        self.file = BufWriter::new(File::create(self.path(TAIL_SUFFIX))?);
        self.part = RunPart::Tail;
        self.written = 0;
        Ok(())
    }

    fn rotate_tail(&mut self) -> io::Result<()> {
        self.file.flush()?;
        let tail = self.path(TAIL_SUFFIX);
        fs::rename(&tail, self.path(PREVIOUS_TAIL_SUFFIX))?;
        self.file = BufWriter::new(File::create(&tail)?);
        self.written = 0;
        Ok(())
    }
}

pub fn newest_run_part(head: &Path) -> PathBuf {
    let tail = head
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(run_stamp_of)
        .map(|stamp| head.with_file_name(stamp.file_name(TAIL_SUFFIX)));
    match tail {
        Some(tail) if tail.is_file() => tail,
        _ => head.to_owned(),
    }
}

fn continuation_line(stamp: &RunStamp) -> String {
    log_line(&Level::INFO, module_path!(), |line| {
        line.push_str("the log continues in ");
        line.push_str(&stamp.file_name(TAIL_SUFFIX));
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const TEST_LIMITS: RunLogLimits = RunLogLimits {
        head: 400,
        tail_half: 300,
        runs: 5,
        total: 2_100,
    };

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "eclipse-diagnostics-{tag}-{:?}",
            std::thread::current().id()
        ));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn at(seconds: u64, millis: u64) -> std::time::SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds) + Duration::from_millis(millis)
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn sorted(mut names: Vec<String>) -> Vec<String> {
        names.sort();
        names
    }

    fn fill(log: &mut RunLog, records: u32) {
        for record in 0..records {
            log.append(format!("record {record:04}\n").as_bytes())
                .unwrap();
        }
    }

    #[test]
    fn init_is_idempotent() {
        super::init(LogSink::Stderr);
        super::init(LogSink::Stderr);
    }

    #[test]
    fn run_stamps_name_the_utc_start_to_the_millisecond() {
        for (time, stamp) in [
            (UNIX_EPOCH, "19700101T000000.000Z"),
            (at(951_782_400, 7), "20000229T000000.007Z"),
            (at(1_709_164_800, 0), "20240229T000000.000Z"),
            (at(1_800_000_000, 692), "20270115T080000.692Z"),
            (at(4_102_444_799, 999), "20991231T235959.999Z"),
            (
                UNIX_EPOCH - Duration::from_millis(86_400_001),
                "19700101T000000.000Z",
            ),
        ] {
            assert_eq!(RunStamp::at(time), RunStamp(stamp.to_owned()));
            assert_eq!(
                run_stamp_of(&format!("eclipse-{stamp}.tail.log.1")),
                Some(RunStamp(stamp.to_owned()))
            );
        }
        for name in [
            "eclipse.log",
            "eclipse.log.next",
            "eclipse-20270115T080000.692Z",
            "eclipse-2027011xT080000.692Z.log",
            "eclipse-20270115T080000.692.log",
            "notes-20270115T080000.692Z.log",
        ] {
            assert_eq!(run_stamp_of(name), None, "{name}");
        }
    }

    #[test]
    fn retention_keeps_the_five_newest_runs_with_their_reports() {
        let dir = temp_dir("retention-count");
        fs::write(dir.join("notes.txt"), b"kept").unwrap();
        let mut stamps = Vec::new();
        for second in 0..7 {
            let mut log = open_run(&dir, at(1_800_000_000 + second, 0), TEST_LIMITS).unwrap();
            log.append(b"one record\n").unwrap();
            fs::write(log.path(".report.txt"), b"report").unwrap();
            stamps.push(log.stamp.clone());
        }

        let kept = stamps[2..]
            .iter()
            .flat_map(|stamp| [HEAD_SUFFIX, ".report.txt"].map(|suffix| stamp.file_name(suffix)));
        let expected = sorted(
            kept.chain([LATEST_LOG.to_owned(), "notes.txt".to_owned()])
                .collect(),
        );
        assert_eq!(names(&dir), expected);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn retention_deletes_the_oldest_whole_runs_to_stay_within_the_byte_cap() {
        let dir = temp_dir("retention-bytes");
        let mut stamps = Vec::new();
        for second in 0..4 {
            let mut log = open_run(&dir, at(1_800_000_000 + second, 0), TEST_LIMITS).unwrap();
            fill(&mut log, 200);
            stamps.push(log.stamp.clone());
        }

        let kept = stamps[2..].iter().flat_map(|stamp| {
            [HEAD_SUFFIX, TAIL_SUFFIX, PREVIOUS_TAIL_SUFFIX].map(|suffix| stamp.file_name(suffix))
        });
        let expected = sorted(kept.chain([LATEST_LOG.to_owned()]).collect());
        assert_eq!(names(&dir), expected);
        let total: u64 = run_logs(&dir).unwrap().iter().map(|run| run.bytes).sum();
        assert!(total <= TEST_LIMITS.total, "{total} bytes");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn older_run_logs_are_every_run_but_the_newest() {
        let dir = temp_dir("older-runs");
        assert_eq!(
            older_run_logs(&dir.join("missing")).unwrap(),
            Vec::<PathBuf>::new()
        );
        fs::write(dir.join("notes.txt"), b"kept").unwrap();
        let mut stamps = Vec::new();
        for second in 0..3 {
            let mut log = open_run(&dir, at(1_800_000_000 + second, 0), TEST_LIMITS).unwrap();
            fill(&mut log, 4);
            fs::write(log.path(".report.txt"), b"report").unwrap();
            stamps.push(log.stamp.clone());
        }

        let older: Vec<String> = older_run_logs(&dir)
            .unwrap()
            .iter()
            .map(|path| path.file_name().unwrap().to_str().unwrap().to_owned())
            .collect();
        let expected = stamps[..2]
            .iter()
            .flat_map(|stamp| [HEAD_SUFFIX, ".report.txt"].map(|suffix| stamp.file_name(suffix)));
        assert_eq!(sorted(older), sorted(expected.collect()));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn eclipse_log_links_to_the_newest_run() {
        let dir = temp_dir("latest");
        for second in 0..2 {
            fs::write(dir.join(LEGACY_PREVIOUS_LOG), b"left by an older Eclipse\n").unwrap();
            let mut log = open_run(&dir, at(1_800_000_000 + second, 0), TEST_LIMITS).unwrap();
            assert!(!dir.join(LEGACY_PREVIOUS_LOG).exists());
            log.append(format!("run {second}\n").as_bytes()).unwrap();
            log.flush().unwrap();
            assert_eq!(
                fs::read_link(dir.join(LATEST_LOG)).unwrap(),
                Path::new(&log.stamp.file_name(HEAD_SUFFIX))
            );
            assert_eq!(
                fs::read_to_string(dir.join(LATEST_LOG)).unwrap(),
                format!("run {second}\n")
            );
        }
        assert!(!dir.join(LATEST_LOG_STAGING).exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_legacy_log_becomes_a_dated_run() {
        let dir = temp_dir("legacy");
        let legacy = dir.join(LATEST_LOG);
        fs::write(&legacy, b"last launch\n").unwrap();
        File::options()
            .write(true)
            .open(&legacy)
            .unwrap()
            .set_modified(at(1_800_000_000, 692))
            .unwrap();
        fs::write(dir.join(LEGACY_PREVIOUS_LOG), b"last launch, earlier\n").unwrap();

        let log = open_run(&dir, at(1_800_000_060, 0), TEST_LIMITS).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("eclipse-20270115T080000.692Z.log")).unwrap(),
            "last launch\n"
        );
        assert!(!dir.join(LEGACY_PREVIOUS_LOG).exists());
        assert_eq!(
            fs::read_link(&legacy).unwrap(),
            Path::new("eclipse-20270115T080100.000Z.log")
        );
        assert_eq!(log.stamp, RunStamp("20270115T080100.000Z".to_owned()));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_head_keeps_the_first_records_and_the_tail_the_newest_within_the_cap() {
        let dir = temp_dir("head-tail");
        let mut log = open_run(&dir, at(1_800_000_000, 692), TEST_LIMITS).unwrap();
        fill(&mut log, 200);
        log.flush().unwrap();

        let head = fs::read_to_string(log.path(HEAD_SUFFIX)).unwrap();
        let previous = fs::read_to_string(log.path(PREVIOUS_TAIL_SUFFIX)).unwrap();
        let tail = fs::read_to_string(log.path(TAIL_SUFFIX)).unwrap();
        assert!(head.starts_with("record 0000\nrecord 0001\n"), "{head}");
        assert!(
            head.ends_with(
                "  INFO eclipse::diagnostics: the log continues in \
                 eclipse-20270115T080000.692Z.tail.log\n"
            ),
            "{head}"
        );
        let newest: Vec<u32> = previous
            .lines()
            .chain(tail.lines())
            .map(|line| line["record ".len()..].parse().unwrap())
            .collect();
        assert_eq!(newest.last(), Some(&199));
        assert!(
            newest.windows(2).all(|pair| pair[1] == pair[0] + 1),
            "{newest:?}"
        );
        assert!(head.len() as u64 <= TEST_LIMITS.head, "{}", head.len());
        assert!(previous.len() as u64 <= TEST_LIMITS.tail_half);
        assert!(tail.len() as u64 <= TEST_LIMITS.tail_half);
        let run: u64 = run_logs(&dir).unwrap().iter().map(|run| run.bytes).sum();
        assert!(run <= TEST_LIMITS.run_cap(), "{run} bytes");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_newest_part_of_a_run_is_its_head_until_the_log_continues_in_the_tail() {
        let dir = temp_dir("newest-part");
        let mut log = open_run(&dir, at(1_800_000_000, 692), TEST_LIMITS).unwrap();
        let head = log.head_path();
        fill(&mut log, 10);
        log.flush().unwrap();
        assert_eq!(newest_run_part(&head), head);

        fill(&mut log, 30);
        log.flush().unwrap();
        let tail = log.path(TAIL_SUFFIX);
        assert_eq!(newest_run_part(&head), tail);
        assert!(!log.path(PREVIOUS_TAIL_SUFFIX).exists());

        fill(&mut log, 100);
        log.flush().unwrap();
        assert!(log.path(PREVIOUS_TAIL_SUFFIX).exists());
        assert_eq!(newest_run_part(&head), tail);
        assert!(
            fs::read_to_string(&tail)
                .unwrap()
                .ends_with("record 0099\n"),
            "the newest part holds the last record"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn raw_lines_lose_nul_bytes_and_long_lines_are_split() {
        let dir = temp_dir("raw");
        let mut log =
            RunLog::create(&dir, RunStamp::at(at(1_800_000_000, 0)), RUN_LOG_LIMITS).unwrap();
        log.append_raw(RawStream::Stderr, b"\0W/eclipse (    2): art line")
            .unwrap();
        log.append_raw(RawStream::Stdout, &vec![b'x'; 100 * 1024])
            .unwrap();
        let wide = format!("a{}", "\u{e9}".repeat(40_000));
        log.append_raw(RawStream::Stdout, wide.as_bytes()).unwrap();
        log.flush().unwrap();

        let text = fs::read_to_string(log.path(HEAD_SUFFIX)).unwrap();
        let records: Vec<&str> = text.lines().collect();
        assert_eq!(records.len(), 5);
        assert!(!text.contains('\0'));
        assert!(
            records[0].ends_with("  INFO stderr: W/eclipse (    2): art line"),
            "{}",
            records[0]
        );
        for (record, width) in records[1..3].iter().zip([64 * 1024, 36 * 1024]) {
            assert!(record.ends_with(&format!("  INFO stdout: {}", "x".repeat(width))));
        }
        assert_eq!(text.matches('\u{e9}').count(), 40_000);
        fs::remove_dir_all(&dir).ok();
    }

    const SINK_CHILD: &str = "ECLIPSE_TEST_DIAGNOSTICS_SINK_CHILD";

    #[test]
    fn the_supervisor_sink_takes_events_and_status_records_as_whole_lines() {
        let Some(path) = std::env::var_os(SINK_CHILD) else {
            let dir = temp_dir("sink");
            let path = dir.join("records");
            let child = crate::bounded_child::output(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "diagnostics::tests::\
                         the_supervisor_sink_takes_events_and_status_records_as_whole_lines",
                        "--test-threads=1",
                    ])
                    .env(SINK_CHILD, &path)
                    .env("RUST_LOG", "info"),
                Duration::from_secs(60),
            );
            let records = fs::read_to_string(&path);
            fs::remove_dir_all(&dir).ok();
            let stderr = String::from_utf8_lossy(&child.stderr);
            assert!(child.status.success(), "{stderr}");
            assert!(!stderr.contains("Runtime aborting"), "{stderr}");
            let records = records.unwrap();
            let heads: Vec<RecordHead<'_>> = records.lines().filter_map(record_head).collect();
            assert_eq!(
                heads,
                [
                    RecordHead {
                        level: Level::ERROR,
                        target: "liblog",
                        message: "Runtime aborting tag=\"art\"",
                    },
                    RecordHead {
                        level: Level::WARN,
                        target: STATUS_TARGET,
                        message: "could not update Roblox",
                    },
                ],
                "{records}"
            );
            assert_eq!(records.lines().count(), 2, "{records}");
            return;
        };
        init(LogSink::Supervisor(File::create(path).unwrap()));
        tracing::error!(target: "liblog", tag = "art", "Runtime aborting");
        record_status(Level::WARN, "could not update Roblox");
    }

    #[test]
    fn record_heads_name_the_level_target_and_message() {
        let line = log_line(&Level::INFO, "eclipse::supervisor", |line| {
            line.push_str("Roblox closed: a: b")
        });
        assert_eq!(
            record_head(line.trim_end()),
            Some(RecordHead {
                level: Level::INFO,
                target: "eclipse::supervisor",
                message: "Roblox closed: a: b",
            })
        );
        for line in [
            "",
            "    at com.roblox.client.ActivityNativeMain.onCreate",
            "2026-10-03T01:02:03.123456Z",
            "2026-10-03T01:02:03.123Z  INFO stderr: short stamp",
            "2026-10-03T01:02:03.123456Z NOTICE stderr: unknown level",
            "2026-10-03T01:02:03.123456Z  INFO no target separator",
        ] {
            assert_eq!(record_head(line), None, "{line}");
        }
    }

    #[test]
    fn a_run_log_keeps_lines_raw_output_and_records_and_hides_join_codes() {
        let dir = temp_dir("run-log");
        let mut log = RunLog::start(&dir).unwrap();
        let head = log.head_path();
        assert_eq!(
            fs::canonicalize(dir.join(LOG_DIR).join(LATEST_LOG)).unwrap(),
            fs::canonicalize(&head).unwrap()
        );

        log.append_line(b"2026-10-03T01:02:03.123456Z ERROR liblog: Runtime aborting")
            .unwrap();
        log.append_raw(
            RawStream::Stdout,
            b"intent roblox://placeId=1818&accessCode=8f3c2a10-5b6d-4e7f-9a1b-2c3d4e5f6a7b",
        )
        .unwrap();
        log.record(Level::ERROR, "eclipse::supervisor", "Roblox crashed")
            .unwrap();
        assert_eq!(fs::read_to_string(&head).unwrap(), "");
        log.flush().unwrap();

        let text = fs::read_to_string(&head).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert_eq!(
            lines[0],
            "2026-10-03T01:02:03.123456Z ERROR liblog: Runtime aborting"
        );
        assert!(
            lines[1].ends_with("  INFO stdout: intent roblox://placeId=1818&accessCode=<redacted>"),
            "{}",
            lines[1]
        );
        assert!(
            lines[2].ends_with(" ERROR eclipse::supervisor: Roblox crashed"),
            "{}",
            lines[2]
        );
        fs::remove_dir_all(&dir).ok();
    }
}
