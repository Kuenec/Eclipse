use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

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
const LOG_FILE: &str = "eclipse.log";
const PREVIOUS_LOG_SUFFIX: &str = ".1";
const LOG_CAP_BYTES: u64 = 8 * 1024 * 1024;
const STATUS_TARGET: &str = "eclipse::status";

static RUN_LOG: Mutex<Option<RunLog>> = Mutex::new(None);

struct PanicSafeStderr;

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

impl<S: Subscriber> Layer<S> for PanicSafeStderr {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let line = log_line(meta.level(), meta.target(), |line| {
            event.record(&mut EventFieldVisitor(line));
        });
        let _ = std::io::stderr().write_all(line.as_bytes());
        append_to_run_log(&line);
    }
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

pub fn init() {
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();

    let _ = Registry::default()
        .with(filter)
        .with(PanicSafeStderr)
        .try_init();
}

pub fn start_run_log(app_data_dir: &Path) -> io::Result<PathBuf> {
    let dir = app_data_dir.join(LOG_DIR);
    fs::create_dir_all(&dir)?;
    let path = dir.join(LOG_FILE);
    let log = RunLog::create(path.clone(), LOG_CAP_BYTES)?;
    *RUN_LOG.lock().unwrap_or_else(PoisonError::into_inner) = Some(log);
    Ok(path)
}

pub fn record_status(level: Level, text: &str) {
    append_to_run_log(&log_line(&level, STATUS_TARGET, |line| line.push_str(text)));
}

fn append_to_run_log(line: &str) {
    let mut run_log = RUN_LOG.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(log) = run_log.as_mut() else {
        return;
    };
    if let Err(error) = log.append(line.as_bytes()) {
        let _ = writeln!(
            std::io::stderr(),
            "eclipse: stopped writing the log {}: {error}",
            log.path.display()
        );
        *run_log = None;
    }
}

struct RunLog {
    path: PathBuf,
    file: File,
    written: u64,
    cap: u64,
}

impl RunLog {
    fn create(path: PathBuf, cap: u64) -> io::Result<Self> {
        match fs::remove_file(previous_log(&path)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let file = File::create(&path)?;
        Ok(Self {
            path,
            file,
            written: 0,
            cap,
        })
    }

    fn append(&mut self, line: &[u8]) -> io::Result<()> {
        let length = line.len() as u64;
        if self.written > 0 && self.written.saturating_add(length) > self.cap {
            fs::rename(&self.path, previous_log(&self.path))?;
            self.file = File::create(&self.path)?;
            self.written = 0;
        }
        self.file.write_all(line)?;
        self.written += length;
        Ok(())
    }
}

fn previous_log(path: &Path) -> PathBuf {
    let mut previous = path.as_os_str().to_owned();
    previous.push(PREVIOUS_LOG_SUFFIX);
    PathBuf::from(previous)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "eclipse-diagnostics-{tag}-{:?}",
            std::thread::current().id()
        ));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn init_is_idempotent() {
        super::init();
        super::init();
    }

    #[test]
    fn the_run_log_keeps_the_latest_lines_within_twice_its_cap() {
        let dir = temp_dir("rotation");
        let path = dir.join(LOG_FILE);
        fs::write(&path, b"last run\n").unwrap();
        fs::write(previous_log(&path), b"last run, earlier\n").unwrap();

        let mut log = RunLog::create(path.clone(), 10).unwrap();
        assert!(!previous_log(&path).exists(), "each run starts afresh");
        assert_eq!(fs::read(&path).unwrap(), b"");
        for line in [
            "first\n",
            "second\n",
            "third\n",
            "a line longer than the cap\n",
        ] {
            log.append(line.as_bytes()).unwrap();
        }
        assert_eq!(fs::read(&path).unwrap(), b"a line longer than the cap\n");
        assert_eq!(fs::read(previous_log(&path)).unwrap(), b"third\n");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn events_and_launch_status_reach_the_run_log() {
        let dir = temp_dir("run-log");
        let path = start_run_log(&dir).unwrap();
        assert_eq!(path, dir.join(LOG_DIR).join(LOG_FILE));

        let subscriber = Registry::default().with(PanicSafeStderr);
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(target: "liblog", tag = "art", "Runtime aborting");
        });
        record_status(Level::WARN, "could not update Roblox");

        let log = fs::read_to_string(&path).unwrap();
        assert!(
            log.contains(" ERROR liblog: Runtime aborting tag=\"art\"\n"),
            "{log}"
        );
        assert!(
            log.contains("  WARN eclipse::status: could not update Roblox\n"),
            "{log}"
        );
        fs::remove_dir_all(&dir).ok();
    }
}
