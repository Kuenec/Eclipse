use std::fmt;
use std::fs::File;
use std::io::{self, Read as _, Write};
use std::os::fd::{AsFd, AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd, RawFd};
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use eclipse::diagnostics::{
    newest_run_part, record_head, RawStream, RecordHead, RunLog, RAW_LINE_BYTES, STATUS_TARGET,
};
use eclipse::framework::lifecycle::{report_exit_to, ClientEnd};
use eclipse::links::redact_join_secrets;
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::io::{Errno, FdFlags};
use rustix::process::{Pid, WaitId, WaitIdOptions};
use tracing::Level;

const FDS_ENV: &str = "ECLIPSE_SUPERVISOR_FDS";
const RUN_LOG_ENV: &str = "ECLIPSE_SUPERVISOR_RUN_LOG";
const TARGET: &str = "eclipse::supervisor";
const PIPE_BYTES: usize = 1024 * 1024;
const READ_CHUNK: usize = 64 * 1024;
const EXIT_CHECK: Duration = Duration::from_secs(5);
const LATE_OUTPUT: Duration = Duration::from_secs(1);
const UNCAUGHT_EXCEPTION_WINDOW: Duration = Duration::from_secs(5);
const UNCAUGHT_EXCEPTION_HEADER: &str = "Exception in thread \"";
const CAUSE_HEADER: &str = "Caused by: ";
const ART_FATAL_SIGNAL: &[u8] = b"Fatal signal ";
const NO_SUCH_FIELD: &str = "java.lang.NoSuchFieldError: ";
const NO_SUCH_METHOD: &str = "java.lang.NoSuchMethodError: ";
const NO_CLASS_DEF: &str = "java.lang.NoClassDefFoundError: Failed resolution of: ";
const FACT_BYTES: usize = 400;
const EXIT_RECORD_BYTES: usize = 4096;
const SIGNAL_EXIT_BASE: i32 = 128;
const PROC_CGROUP: &str = "/proc/self/cgroup";
const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const MEMORY_EVENTS: &str = "memory.events";

pub(crate) struct Supervision {
    pub(crate) records: File,
    pub(crate) run_log: PathBuf,
}

pub(crate) fn adopt() -> Result<Option<Supervision>, String> {
    let Some(fds) = std::env::var_os(FDS_ENV) else {
        return Ok(None);
    };
    let run_log = std::env::var_os(RUN_LOG_ENV);
    unsafe {
        std::env::remove_var(FDS_ENV);
        std::env::remove_var(RUN_LOG_ENV);
    }
    let malformed = || format!("{FDS_ENV} must name the supervisor's two pipes as RECORDS,EXIT");
    let (records, exit) = fds
        .to_str()
        .and_then(|fds| fds.split_once(','))
        .ok_or_else(malformed)?;
    let records: RawFd = records.parse().map_err(|_| malformed())?;
    let exit: RawFd = exit.parse().map_err(|_| malformed())?;
    if records == exit {
        return Err(malformed());
    }
    let run_log = run_log
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| format!("{RUN_LOG_ENV} must name the run log"))?;
    let records = adopt_pipe(records)?;
    report_exit_to(adopt_pipe(exit)?);
    Ok(Some(Supervision { records, run_log }))
}

fn adopt_pipe(fd: RawFd) -> Result<File, String> {
    let not_a_pipe = |reason: String| {
        format!("{FDS_ENV} names fd {fd}, which is not a supervisor pipe: {reason}")
    };
    if fd <= libc::STDERR_FILENO {
        return Err(not_a_pipe("it is a standard stream".to_owned()));
    }
    let mut status = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, status.as_mut_ptr()) } != 0 {
        return Err(not_a_pipe(io::Error::last_os_error().to_string()));
    }
    if unsafe { status.assume_init() }.st_mode & libc::S_IFMT != libc::S_IFIFO {
        return Err(not_a_pipe("it is not a pipe".to_owned()));
    }
    let pipe = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    rustix::io::fcntl_setfd(&pipe, FdFlags::CLOEXEC)
        .map_err(|error| not_a_pipe(format!("it cannot be closed on exec: {error}")))?;
    Ok(pipe)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Signal(i32);

impl Signal {
    fn name(self) -> Option<&'static str> {
        Some(match self.0 {
            libc::SIGHUP => "SIGHUP",
            libc::SIGINT => "SIGINT",
            libc::SIGQUIT => "SIGQUIT",
            libc::SIGILL => "SIGILL",
            libc::SIGTRAP => "SIGTRAP",
            libc::SIGABRT => "SIGABRT",
            libc::SIGBUS => "SIGBUS",
            libc::SIGFPE => "SIGFPE",
            libc::SIGKILL => "SIGKILL",
            libc::SIGSEGV => "SIGSEGV",
            libc::SIGPIPE => "SIGPIPE",
            libc::SIGTERM => "SIGTERM",
            libc::SIGSYS => "SIGSYS",
            _ => return None,
        })
    }

    fn cause(self) -> Option<&'static str> {
        Some(match self.0 {
            libc::SIGILL => "an illegal instruction or a deliberate trap in Roblox's code",
            libc::SIGTRAP => "a trap in Roblox's code",
            libc::SIGABRT => "Roblox aborted",
            libc::SIGBUS => "invalid memory access to a mapped file",
            libc::SIGFPE => "an arithmetic error",
            libc::SIGSEGV => "invalid memory access",
            libc::SIGPIPE => "a write to a closed pipe",
            libc::SIGSYS => "a system call the system refused",
            _ => return None,
        })
    }
}

impl fmt::Display for Signal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "signal {}", self.0)?;
        if let Some(name) = self.name() {
            write!(f, ", {name}")?;
        }
        if let Some(cause) = self.cause() {
            write!(f, ": {cause}")?;
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RunEnd {
    Ended,
    FailureShown,
    Stopped(Signal),
    Killed { oom_kills: Option<u64> },
    Crashed(Signal),
    ClosedItself,
    ExitedUnexpectedly { status: i32 },
}

impl RunEnd {
    fn shown(&self) -> bool {
        match self {
            Self::Killed { .. } | Self::Crashed(_) | Self::ExitedUnexpectedly { .. } => true,
            Self::Ended | Self::FailureShown | Self::Stopped(_) | Self::ClosedItself => false,
        }
    }

    fn failed(&self) -> bool {
        self.shown() || *self == Self::FailureShown
    }
}

impl fmt::Display for RunEnd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ended => f.write_str("Roblox ended without an error"),
            Self::FailureShown => f.write_str("Roblox stopped after Eclipse reported why"),
            Self::Stopped(signal) => write!(f, "Roblox was stopped ({signal})"),
            Self::Killed {
                oom_kills: Some(kills),
            } if *kills > 0 => f.write_str(
                "Roblox was killed (SIGKILL); the kernel's out-of-memory killer stopped it",
            ),
            Self::Killed { oom_kills: Some(_) } => f.write_str("Roblox was killed (SIGKILL)"),
            Self::Killed { oom_kills: None } => {
                f.write_str("Roblox was killed (SIGKILL); this is often the out-of-memory killer")
            }
            Self::Crashed(signal) => write!(f, "Roblox crashed ({signal})"),
            Self::ClosedItself => f.write_str("Roblox closed itself"),
            Self::ExitedUnexpectedly { status } => {
                write!(f, "Roblox exited unexpectedly with status {status}")
            }
        }
    }
}

pub(crate) fn classify(
    record: Option<ClientEnd>,
    status: ExitStatus,
    uncaught_exception_recent: bool,
    art_fatal_signal: Option<Signal>,
    oom_kills: Option<u64>,
) -> RunEnd {
    if let Some(end) = record {
        return match end {
            ClientEnd::FailureShown => RunEnd::FailureShown,
            ClientEnd::Played | ClientEnd::WindowClosed | ClientEnd::ClosedForAnotherLaunch => {
                RunEnd::Ended
            }
            ClientEnd::ClientExited { status } => {
                exit_status_end(status, uncaught_exception_recent)
            }
        };
    }
    if let Some(signal) = status.signal() {
        return match signal {
            libc::SIGINT | libc::SIGTERM | libc::SIGHUP => RunEnd::Stopped(Signal(signal)),
            libc::SIGKILL => RunEnd::Killed { oom_kills },
            _ => RunEnd::Crashed(Signal(signal)),
        };
    }
    if let Some(signal) = art_fatal_signal {
        return RunEnd::Crashed(signal);
    }
    match status.code() {
        Some(code) => exit_status_end(code, uncaught_exception_recent),
        None => RunEnd::ExitedUnexpectedly {
            status: status.into_raw(),
        },
    }
}

fn exit_status_end(status: i32, uncaught_exception_recent: bool) -> RunEnd {
    if status == 0 && !uncaught_exception_recent {
        RunEnd::ClosedItself
    } else {
        RunEnd::ExitedUnexpectedly { status }
    }
}

pub(crate) struct Finished {
    pub(crate) end: RunEnd,
    status_error: Option<String>,
    missing_android_api: Option<String>,
    status: ExitStatus,
    pub(crate) log: PathBuf,
}

impl Finished {
    pub(crate) fn failure(&self) -> Option<String> {
        self.end.shown().then(|| self.outcome())
    }

    fn outcome(&self) -> String {
        let mut lines = vec![self.end.to_string()];
        lines.extend(
            self.status_error
                .iter()
                .map(|error| format!("Last error: {error}")),
        );
        lines.extend(
            self.missing_android_api
                .iter()
                .map(|api| format!("Missing Android API: {api}")),
        );
        lines.join("\n")
    }

    pub(crate) fn exit_code(&self) -> ExitCode {
        let code = self
            .status
            .code()
            .or_else(|| self.status.signal().map(|signal| SIGNAL_EXIT_BASE + signal))
            .unwrap_or(i32::from(u8::MAX));
        ExitCode::from(u8::try_from(code).unwrap_or(u8::MAX))
    }
}

pub(crate) struct Output<O, E> {
    pub(crate) stdout: O,
    pub(crate) stderr: E,
    pub(crate) echo_records: bool,
}

pub(crate) fn run<O: Write, E: Write>(
    mut command: Command,
    lock: File,
    runtime_dir: &Path,
    log: RunLog,
    output: Output<O, E>,
) -> Result<Finished, String> {
    let log_path = log.head_path();
    let mut sink = Sink {
        log: Some(log),
        log_path: log_path.clone(),
        stdout: Some(output.stdout),
        stderr: Some(output.stderr),
        echo_records: output.echo_records,
        echoing: false,
        facts: Facts::default(),
    };
    let oom_kills_before = oom_kills();
    let (mut child, streams) = match spawn(&mut command, &mut sink) {
        Ok(spawned) => spawned,
        Err(error) => {
            let message = format!("Eclipse could not start Roblox's process: {error}");
            sink.footer(Level::ERROR, &message);
            return Err(message);
        }
    };
    let mut drain = Drain {
        streams,
        stdout_lines: Lines::default(),
        stderr_lines: Lines::default(),
        record_lines: Lines::default(),
        exit_record: Vec::new(),
        sink,
    };
    let waited = drain.until_exit(&mut child);
    if let Err(error) = eclipse::session::clear(runtime_dir) {
        drain.sink.note(Level::WARN, &error.to_string());
    }
    drop(lock);
    let (status, gone_at) = match waited {
        Ok(waited) => waited,
        Err(error) => {
            let message = format!("Eclipse lost track of Roblox's process: {error}");
            drain.sink.footer(Level::ERROR, &message);
            return Err(message);
        }
    };
    let record = drain.exit_record();
    let mut sink = drain.sink;
    let uncaught_exception_recent = sink
        .facts
        .uncaught_exception_at
        .is_some_and(|at| gone_at.saturating_duration_since(at) <= UNCAUGHT_EXCEPTION_WINDOW);
    let oom_kills = oom_kills_before
        .zip(oom_kills())
        .map(|(before, after)| after.saturating_sub(before));
    let art_fatal_signal = sink.facts.art_fatal_signal.map(|(signal, _)| signal);
    let end = classify(
        record,
        status,
        uncaught_exception_recent,
        art_fatal_signal,
        oom_kills,
    );
    let status_error = end
        .failed()
        .then(|| sink.facts.last_status_error.take())
        .flatten();
    let missing_android_api = end
        .failed()
        .then(|| sink.facts.missing_android_api_before_the_end(gone_at))
        .flatten();
    let mut finished = Finished {
        end,
        status_error,
        missing_android_api,
        status,
        log: log_path,
    };
    let level = if finished.end.shown() {
        Level::ERROR
    } else {
        Level::INFO
    };
    sink.footer(level, &finished.outcome());
    finished.log = newest_run_part(&finished.log);
    Ok(finished)
}

fn spawn<O: Write, E: Write>(
    command: &mut Command,
    sink: &mut Sink<O, E>,
) -> io::Result<(Child, Streams)> {
    let (records, records_end) = io::pipe()?;
    let (exit, exit_end) = io::pipe()?;
    sink.enlarge(&records, "log record");
    let parent = rustix::process::getpid();
    let inherited = [records_end.as_raw_fd(), exit_end.as_raw_fd()];
    command
        .env(FDS_ENV, format!("{},{}", inherited[0], inherited[1]))
        .env(RUN_LOG_ENV, &sink.log_path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    unsafe {
        command.pre_exec(move || inherit_supervision(parent, inherited));
    }
    let mut child = command.spawn()?;
    drop((records_end, exit_end));
    let stdout = child.stdout.take().map(OwnedFd::from).map(File::from);
    let stderr = child.stderr.take().map(OwnedFd::from).map(File::from);
    if let Some(stderr) = &stderr {
        sink.enlarge(stderr, "stderr");
    }
    let streams = Streams([
        stdout,
        stderr,
        Some(File::from(OwnedFd::from(records))),
        Some(File::from(OwnedFd::from(exit))),
    ]);
    Ok((child, streams))
}

fn inherit_supervision(parent: Pid, inherited: [RawFd; 2]) -> io::Result<()> {
    for fd in inherited {
        rustix::io::fcntl_setfd(unsafe { BorrowedFd::borrow_raw(fd) }, FdFlags::empty())?;
    }
    rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::TERM))?;
    if rustix::process::getppid() != Some(parent) {
        return Err(Errno::SRCH.into());
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Stdout,
    Stderr,
    Records,
    Exit,
}

impl Source {
    const ALL: [Self; 4] = [Self::Stdout, Self::Stderr, Self::Records, Self::Exit];

    fn name(self) -> &'static str {
        match self {
            Self::Stdout => "standard output",
            Self::Stderr => "standard error",
            Self::Records => "log records",
            Self::Exit => "exit record",
        }
    }
}

struct Streams([Option<File>; 4]);

impl Streams {
    fn is_open(&self, source: Source) -> bool {
        self.0[source as usize].is_some()
    }

    fn any_open(&self) -> bool {
        self.0.iter().any(Option::is_some)
    }

    fn close(&mut self, source: Source) {
        self.0[source as usize] = None;
    }

    fn read(&mut self, source: Source, buffer: &mut [u8]) -> io::Result<usize> {
        match &mut self.0[source as usize] {
            Some(stream) => stream.read(buffer),
            None => Ok(0),
        }
    }

    fn ready(&self, timeout: Duration) -> io::Result<Vec<Source>> {
        let open: Vec<(Source, &File)> = Source::ALL
            .into_iter()
            .filter_map(|source| Some((source, self.0[source as usize].as_ref()?)))
            .collect();
        let mut fds: Vec<PollFd<'_>> = open
            .iter()
            .map(|(_, stream)| PollFd::new(*stream, PollFlags::IN))
            .collect();
        let timeout = Timespec::try_from(timeout).map_err(|_| io::ErrorKind::InvalidInput)?;
        match rustix::event::poll(&mut fds, Some(&timeout)) {
            Ok(_) | Err(Errno::INTR) => {}
            Err(error) => return Err(error.into()),
        }
        Ok(open
            .iter()
            .zip(&fds)
            .filter(|(_, fd)| !fd.revents().is_empty())
            .map(|((source, _), _)| *source)
            .collect())
    }
}

#[derive(Default)]
struct Lines {
    partial: Vec<u8>,
}

impl Lines {
    fn feed(&mut self, mut chunk: &[u8], mut line: impl FnMut(&[u8])) {
        while let Some(end) = chunk.iter().position(|&byte| byte == b'\n') {
            if self.partial.is_empty() {
                line(&chunk[..end]);
            } else {
                self.partial.extend_from_slice(&chunk[..end]);
                line(&self.partial);
                self.partial.clear();
            }
            chunk = &chunk[end + 1..];
        }
        self.partial.extend_from_slice(chunk);
        if self.partial.len() >= RAW_LINE_BYTES {
            self.finish(line);
        }
    }

    fn finish(&mut self, mut line: impl FnMut(&[u8])) {
        if !self.partial.is_empty() {
            line(&self.partial);
            self.partial.clear();
        }
    }
}

#[derive(Default)]
struct Facts {
    last_status_error: Option<String>,
    uncaught_exception_at: Option<Instant>,
    art_fatal_signal: Option<(Signal, Instant)>,
    missing_android_api: Option<(String, Instant)>,
}

impl Facts {
    fn note_stderr(&mut self, line: &[u8], at: Instant) {
        if line.starts_with(UNCAUGHT_EXCEPTION_HEADER.as_bytes()) {
            self.uncaught_exception_at = Some(at);
        }
        if self.art_fatal_signal.is_some() {
            return;
        }
        if let Some(signal) = art_fatal_signal(line) {
            self.art_fatal_signal = Some((signal, at));
        } else if let Some(api) = missing_android_api(line) {
            self.missing_android_api = Some((api, at));
        }
    }

    fn missing_android_api_before_the_end(&mut self, gone_at: Instant) -> Option<String> {
        let end = self.art_fatal_signal.map_or(gone_at, |(_, at)| at);
        let (api, at) = self.missing_android_api.take()?;
        (end.saturating_duration_since(at) <= UNCAUGHT_EXCEPTION_WINDOW).then_some(api)
    }
}

fn art_fatal_signal(line: &[u8]) -> Option<Signal> {
    let report = line.strip_prefix(ART_FATAL_SIGNAL)?;
    let digits = report
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    let number = std::str::from_utf8(&report[..digits]).ok()?.parse().ok()?;
    report[digits..]
        .starts_with(b" (")
        .then_some(Signal(number))
}

fn missing_android_api(line: &[u8]) -> Option<String> {
    let line = std::str::from_utf8(line).ok()?;
    let error = match line.strip_prefix(CAUSE_HEADER) {
        Some(cause) => cause,
        None => uncaught_exception(line).unwrap_or(line),
    };
    if let Some(message) = error.strip_prefix(NO_SUCH_FIELD) {
        return missing_field(message);
    }
    if let Some(message) = error.strip_prefix(NO_SUCH_METHOD) {
        return missing_method(message);
    }
    android_class(error.strip_prefix(NO_CLASS_DEF)?)
}

fn missing_field(message: &str) -> Option<String> {
    if let Some(linked) = message.strip_prefix("No ") {
        let (_, field) = linked.split_once("field ")?;
        let (name, declared) = field.split_once(" of type ")?;
        let (_, class) = declared.split_once(" in class ")?;
        return android_member(class, name);
    }
    if let Some(field_type) = message.strip_prefix("no type \"") {
        return android_class(field_type);
    }
    let (_, looked_up) = message.strip_prefix("no \"")?.split_once("\" field \"")?;
    let (name, class) = looked_up.split_once("\" in class \"")?;
    android_member(class, name)
}

fn missing_method(message: &str) -> Option<String> {
    if let Some(linked) = message.strip_prefix("No ") {
        let (_, method) = linked.split_once(" method ")?;
        let (name, class) = method.split_once(" in class ")?;
        return android_member(class, name);
    }
    let (_, looked_up) = message.strip_prefix("no ")?.split_once(" method \"")?;
    let (_, method) = looked_up.split_once(";.")?;
    let (name, _) = method.split_once('"')?;
    android_member(looked_up, name)
}

fn uncaught_exception(line: &str) -> Option<&str> {
    let (_, error) = line
        .strip_prefix(UNCAUGHT_EXCEPTION_HEADER)?
        .split_once("\" ")?;
    Some(error)
}

fn android_member(class: &str, member: &str) -> Option<String> {
    Some(format!("{}.{member}", android_class(class)?))
}

fn android_class(descriptor: &str) -> Option<String> {
    let (class, _) = descriptor.strip_prefix('L')?.split_once(';')?;
    class
        .starts_with("android/")
        .then(|| class.replace('/', "."))
}

fn without_leading_nul(line: &[u8]) -> &[u8] {
    let start = line
        .iter()
        .position(|&byte| byte != 0)
        .unwrap_or(line.len());
    &line[start..]
}

struct Sink<O, E> {
    log: Option<RunLog>,
    log_path: PathBuf,
    stdout: Option<O>,
    stderr: Option<E>,
    echo_records: bool,
    echoing: bool,
    facts: Facts,
}

impl<O: Write, E: Write> Sink<O, E> {
    fn write_log(&mut self, write: impl FnOnce(&mut RunLog) -> io::Result<()>) {
        let Some(log) = &mut self.log else {
            return;
        };
        if let Err(error) = write(log) {
            self.log = None;
            eprintln!(
                "eclipse: stopped writing the log {}: {error}",
                self.log_path.display()
            );
        }
    }

    fn note(&mut self, level: Level, text: &str) {
        self.write_log(|log| log.record(level, TARGET, text));
    }

    fn footer(&mut self, level: Level, text: &str) {
        self.note(level, text);
        self.write_log(RunLog::flush);
    }

    fn enlarge(&mut self, pipe: impl AsFd, name: &str) {
        if let Err(error) = rustix::pipe::fcntl_setpipe_size(pipe, PIPE_BYTES) {
            self.note(
                Level::WARN,
                &format!(
                    "the {name} pipe keeps its default size, so a slow log can stall Roblox: \
                     {error}"
                ),
            );
        }
    }

    fn forward(&mut self, stream: RawStream, bytes: &[u8]) {
        let copied = match stream {
            RawStream::Stdout => self.stdout.as_mut().map(|out| copy(out, &[bytes])),
            RawStream::Stderr => self.stderr.as_mut().map(|out| copy(out, &[bytes])),
        };
        if let Some(Err(error)) = copied {
            self.stop_forwarding(stream, &error);
        }
    }

    fn stop_forwarding(&mut self, stream: RawStream, error: &io::Error) {
        let name = match stream {
            RawStream::Stdout => {
                self.stdout = None;
                Source::Stdout.name()
            }
            RawStream::Stderr => {
                self.stderr = None;
                Source::Stderr.name()
            }
        };
        self.note(
            Level::WARN,
            &format!("stopped copying Roblox's {name} to Eclipse's: {error}"),
        );
    }

    fn raw(&mut self, stream: RawStream, line: &[u8]) {
        if matches!(stream, RawStream::Stderr) {
            self.facts
                .note_stderr(without_leading_nul(line), Instant::now());
        }
        self.write_log(|log| log.append_raw(stream, line));
    }

    fn record(&mut self, line: &[u8]) {
        let text = String::from_utf8_lossy(line);
        let text = redact_join_secrets(&text);
        if let Some(head) = record_head(&text) {
            self.note_record(&head);
        }
        let line = text.as_bytes();
        if self.echoing {
            if let Some(Err(error)) = self.stderr.as_mut().map(|out| copy(out, &[line, b"\n"])) {
                self.stop_forwarding(RawStream::Stderr, &error);
            }
        }
        self.write_log(|log| log.append_line(line));
    }

    fn note_record(&mut self, head: &RecordHead<'_>) {
        let status = head.target == STATUS_TARGET;
        self.echoing = self.echo_records && !status;
        if status && head.level == Level::ERROR {
            let message = head.message;
            self.facts.last_status_error =
                Some(message[..message.floor_char_boundary(FACT_BYTES)].to_owned());
        }
    }
}

fn copy(out: &mut impl Write, parts: &[&[u8]]) -> io::Result<()> {
    for part in parts {
        out.write_all(part)?;
    }
    out.flush()
}

struct Drain<O, E> {
    streams: Streams,
    stdout_lines: Lines,
    stderr_lines: Lines,
    record_lines: Lines,
    exit_record: Vec<u8>,
    sink: Sink<O, E>,
}

impl<O: Write, E: Write> Drain<O, E> {
    fn until_exit(&mut self, child: &mut Child) -> io::Result<(ExitStatus, Instant)> {
        let client = Pid::from_child(child);
        let mut buffer = vec![0; READ_CHUNK];
        let mut next_check = Instant::now() + EXIT_CHECK;
        while self.streams.is_open(Source::Exit) {
            let now = Instant::now();
            if now >= next_check {
                if exited(client, WaitIdOptions::NOHANG)? {
                    break;
                }
                next_check = now + EXIT_CHECK;
            }
            if !self.drain_ready(&mut buffer, next_check - now) {
                break;
            }
        }
        let gone_at = Instant::now();
        exited(client, WaitIdOptions::empty())?;
        self.end_leftovers(client);
        let late = Instant::now() + LATE_OUTPUT;
        while self.streams.any_open() {
            let Some(left) = late
                .checked_duration_since(Instant::now())
                .filter(|left| !left.is_zero())
            else {
                break;
            };
            if !self.drain_ready(&mut buffer, left) {
                break;
            }
        }
        self.finish_lines();
        Ok((child.wait()?, gone_at))
    }

    fn end_leftovers(&mut self, client: Pid) {
        if let Err(error) =
            rustix::process::kill_process_group(client, rustix::process::Signal::KILL)
        {
            self.sink.note(
                Level::WARN,
                &format!(
                    "processes Roblox's process {client} started may outlive it, because ending \
                     its process group failed: {error}"
                ),
            );
        }
    }

    fn drain_ready(&mut self, buffer: &mut [u8], timeout: Duration) -> bool {
        self.sink.write_log(RunLog::flush);
        let ready = match self.streams.ready(timeout) {
            Ok(ready) => ready,
            Err(error) => {
                self.sink.note(
                    Level::ERROR,
                    &format!("stopped reading Roblox's output: {error}"),
                );
                return false;
            }
        };
        for source in ready {
            match self.streams.read(source, buffer) {
                Ok(0) => self.streams.close(source),
                Ok(read) => self.take(source, &buffer[..read]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    self.streams.close(source);
                    self.sink.note(
                        Level::WARN,
                        &format!("stopped reading Roblox's {}: {error}", source.name()),
                    );
                }
            }
        }
        true
    }

    fn take(&mut self, source: Source, bytes: &[u8]) {
        match source {
            Source::Stdout => {
                self.sink.forward(RawStream::Stdout, bytes);
                self.stdout_lines
                    .feed(bytes, |line| self.sink.raw(RawStream::Stdout, line));
            }
            Source::Stderr => {
                self.sink.forward(RawStream::Stderr, bytes);
                self.stderr_lines
                    .feed(bytes, |line| self.sink.raw(RawStream::Stderr, line));
            }
            Source::Records => self.record_lines.feed(bytes, |line| self.sink.record(line)),
            Source::Exit => {
                let room = EXIT_RECORD_BYTES.saturating_sub(self.exit_record.len());
                self.exit_record
                    .extend_from_slice(&bytes[..bytes.len().min(room)]);
            }
        }
    }

    fn finish_lines(&mut self) {
        self.stdout_lines
            .finish(|line| self.sink.raw(RawStream::Stdout, line));
        self.stderr_lines
            .finish(|line| self.sink.raw(RawStream::Stderr, line));
        self.record_lines.finish(|line| self.sink.record(line));
    }

    fn exit_record(&mut self) -> Option<ClientEnd> {
        let record = self.exit_record.trim_ascii();
        if record.is_empty() {
            return None;
        }
        match serde_json::from_slice(record) {
            Ok(end) => Some(end),
            Err(error) => {
                let text = format!(
                    "ignored an exit record Eclipse cannot read ({error}): {}",
                    String::from_utf8_lossy(record)
                );
                self.sink.note(Level::WARN, &text);
                None
            }
        }
    }
}

fn exited(client: Pid, options: WaitIdOptions) -> io::Result<bool> {
    loop {
        match rustix::process::waitid(
            WaitId::Pid(client),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | options,
        ) {
            Ok(status) => return Ok(status.is_some()),
            Err(Errno::INTR) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

fn oom_kills() -> Option<u64> {
    let cgroup = std::fs::read_to_string(PROC_CGROUP).ok()?;
    let events = Path::new(CGROUP_ROOT)
        .join(unified_cgroup(&cgroup)?.trim_start_matches('/'))
        .join(MEMORY_EVENTS);
    oom_kill_count(&std::fs::read_to_string(events).ok()?)
}

fn unified_cgroup(proc_cgroup: &str) -> Option<&str> {
    proc_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
}

fn oom_kill_count(memory_events: &str) -> Option<u64> {
    memory_events
        .lines()
        .find_map(|line| line.strip_prefix("oom_kill ")?.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use eclipse::diagnostics::LogSink;
    use eclipse::framework::lifecycle::finish_android_process;
    use std::sync::atomic::{AtomicI32, Ordering};

    const CHILD: &str = "ECLIPSE_TEST_SUPERVISED_CHILD";
    const PARENT: &str = "ECLIPSE_TEST_SUPERVISING_PARENT";
    const FLOOD_THREADS: usize = 4;
    const FLOOD_RECORDS: usize = 100_000;
    const FLOOD_LONG_BYTES: usize = 5_000;
    const PIPE_BUF: usize = 4_096;
    const SESSION_FILE: &str = "session.json";

    struct Supervised {
        finished: Finished,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        log: String,
        newest_part: String,
        session_file_left: bool,
    }

    fn test_dir(test: &str) -> PathBuf {
        std::env::temp_dir().join(format!("eclipse-supervisor-{test}"))
    }

    fn temp_dir(test: &str) -> PathBuf {
        let dir = test_dir(test);
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn child_command(test: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", &format!("supervisor::tests::{test}")])
            .env(CHILD, "1")
            .env_remove(PARENT);
        command
    }

    fn lock_in(dir: &Path) -> File {
        File::create(dir.join("client.lock")).unwrap()
    }

    fn supervise(test: &str) -> Supervised {
        let dir = temp_dir(test);
        let log = RunLog::start(&dir).unwrap();
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let output = Output {
            stdout: &mut stdout,
            stderr: &mut stderr,
            echo_records: false,
        };
        let finished = run(child_command(test), lock_in(&dir), &dir, log, output).unwrap();
        let mut log = String::new();
        for suffix in [".log", ".tail.log.1", ".tail.log"] {
            let part = finished
                .log
                .with_file_name(format!("{}{suffix}", stem(&finished.log)));
            if let Ok(text) = std::fs::read_to_string(part) {
                log.push_str(&text);
            }
        }
        let newest_part = std::fs::read_to_string(&finished.log).unwrap();
        let session_file_left = dir.join(SESSION_FILE).exists();
        std::fs::remove_dir_all(&dir).ok();
        Supervised {
            finished,
            stdout,
            stderr,
            log,
            newest_part,
            session_file_left,
        }
    }

    fn stem(part: &Path) -> String {
        let name = part.file_name().unwrap().to_str().unwrap();
        name.strip_suffix(".tail.log")
            .or_else(|| name.strip_suffix(".log"))
            .unwrap()
            .to_owned()
    }

    fn supervised_child() -> bool {
        if std::env::var_os(CHILD).is_none() {
            return false;
        }
        let Supervision { records, .. } = adopt().unwrap().unwrap();
        eclipse::diagnostics::init(LogSink::Supervisor(records));
        true
    }

    fn write_raw(stream: RawStream, bytes: &[u8]) {
        match stream {
            RawStream::Stdout => {
                let mut out = io::stdout();
                out.write_all(bytes).unwrap();
                out.flush().unwrap();
            }
            RawStream::Stderr => io::stderr().write_all(bytes).unwrap(),
        }
    }

    fn die_by(signal: i32) -> ! {
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
            libc::_exit(SIGNAL_EXIT_BASE - 1)
        }
    }

    fn footer(log: &str) -> Vec<&str> {
        let start = log
            .lines()
            .position(|line| line.contains(" eclipse::supervisor: "))
            .unwrap_or_else(|| panic!("no footer in\n{log}"));
        log.lines().skip(start).collect()
    }

    fn exit_status(raw: i32) -> ExitStatus {
        ExitStatus::from_raw(raw)
    }

    fn exited(code: i32) -> ExitStatus {
        exit_status(code << 8)
    }

    #[test]
    fn a_client_that_played_ends_with_its_output_copied_and_every_line_in_the_log() {
        const STDOUT: &[u8] = b"# Booting the ART VM\nboot line two\n";
        const STDERR: &[u8] = b"\0W/eclipse (    2): art line\nlast words without a newline";
        if supervised_child() {
            write_raw(RawStream::Stdout, STDOUT);
            write_raw(RawStream::Stderr, STDERR);
            tracing::info!(target: "liblog", "a client record");
            finish_android_process(ClientEnd::Played);
        }

        let run =
            supervise("a_client_that_played_ends_with_its_output_copied_and_every_line_in_the_log");
        assert_eq!(run.finished.end, RunEnd::Ended);
        assert_eq!(run.finished.failure(), None);
        assert_eq!(run.finished.exit_code(), ExitCode::SUCCESS);
        assert_eq!(run.newest_part, run.log, "a short run is all in its head");
        assert!(run.stdout.ends_with(STDOUT), "{:?}", run.stdout);
        assert_eq!(run.stderr, STDERR);
        assert!(!run.log.contains('\0'), "{}", run.log);
        assert!(
            run.log.lines().all(|line| record_head(line).is_some()),
            "{}",
            run.log
        );
        for expected in [
            "  INFO stdout: # Booting the ART VM",
            "  INFO stdout: boot line two",
            "  INFO stderr: W/eclipse (    2): art line",
            "  INFO stderr: last words without a newline",
            "  INFO liblog: a client record",
            "  INFO eclipse::supervisor: Roblox ended without an error",
        ] {
            assert!(
                run.log.contains(&format!("{expected}\n")),
                "{expected}\n{}",
                run.log
            );
        }
        let logged_stdout: String = run
            .log
            .lines()
            .filter_map(|line| line.split_once("  INFO stdout: ").map(|(_, text)| text))
            .map(|text| format!("{text}\n"))
            .collect();
        assert_eq!(logged_stdout.as_bytes(), run.stdout.as_slice());
    }

    #[test]
    fn a_segfault_is_a_crash_named_with_its_signal() {
        if supervised_child() {
            write_raw(RawStream::Stderr, b"F/libc: about to fault\n");
            die_by(libc::SIGSEGV);
        }

        let run = supervise("a_segfault_is_a_crash_named_with_its_signal");
        assert_eq!(run.finished.end, RunEnd::Crashed(Signal(libc::SIGSEGV)));
        assert_eq!(run.finished.exit_code(), ExitCode::from(139));
        assert!(
            run.log.contains("  INFO stderr: F/libc: about to fault\n"),
            "{}",
            run.log
        );
        let footer = footer(&run.log);
        assert_eq!(footer.len(), 1, "{footer:?}");
        assert!(
            footer[0].ends_with(
                " ERROR eclipse::supervisor: Roblox crashed (signal 11, SIGSEGV: invalid memory \
                 access)"
            ),
            "{footer:?}"
        );
        assert_eq!(
            run.finished.failure().as_deref(),
            Some("Roblox crashed (signal 11, SIGSEGV: invalid memory access)")
        );
    }

    #[test]
    fn the_session_file_is_removed_once_the_client_is_gone_even_after_a_crash() {
        const TEST: &str = "the_session_file_is_removed_once_the_client_is_gone_even_after_a_crash";
        if supervised_child() {
            std::fs::write(test_dir(TEST).join(SESSION_FILE), "{}\n").unwrap();
            die_by(libc::SIGSEGV);
        }

        let run = supervise(TEST);
        assert_eq!(run.finished.end, RunEnd::Crashed(Signal(libc::SIGSEGV)));
        assert!(!run.session_file_left);
    }

    #[test]
    fn android_and_engine_errors_before_a_crash_are_never_its_detail() {
        if supervised_child() {
            tracing::error!(
                target: "android.util.Log",
                tag = "CookieProtocol",
                "Failed to update WebViewCookieHandler"
            );
            tracing::error!(target: "liblog", "Assertion failed: renderer");
            write_raw(RawStream::Stderr, b"aborting\n");
            std::process::abort();
        }

        let run = supervise("android_and_engine_errors_before_a_crash_are_never_its_detail");
        assert_eq!(run.finished.end, RunEnd::Crashed(Signal(libc::SIGABRT)));
        for record in [
            " ERROR android.util.Log: Failed to update WebViewCookieHandler tag=\"CookieProtocol\"\n",
            " ERROR liblog: Assertion failed: renderer\n",
        ] {
            assert!(run.log.contains(record), "{record}\n{}", run.log);
        }
        assert_eq!(footer(&run.log).len(), 1, "{}", run.log);
        assert_eq!(
            run.finished.failure().as_deref(),
            Some("Roblox crashed (signal 6, SIGABRT: Roblox aborted)")
        );
    }

    #[test]
    fn a_status_error_is_the_detail_before_any_other_error() {
        if supervised_child() {
            eclipse::diagnostics::record_status(Level::ERROR, "Roblox stopped: out of memory");
            tracing::error!(target: "liblog", "a later engine error");
            unsafe { libc::_exit(1) }
        }

        let run = supervise("a_status_error_is_the_detail_before_any_other_error");
        assert_eq!(run.finished.end, RunEnd::ExitedUnexpectedly { status: 1 });
        assert_eq!(run.finished.exit_code(), ExitCode::FAILURE);
        assert_eq!(
            run.finished.failure().as_deref(),
            Some(
                "Roblox exited unexpectedly with status 1\nLast error: Roblox stopped: out of \
                 memory"
            )
        );
    }

    fn ended(pid: libc::pid_t) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
            stat.rsplit_once(") ")
                .is_some_and(|(_, fields)| fields.starts_with('Z'))
        })
    }

    #[test]
    fn processes_a_crashed_client_leaves_behind_end_with_it() {
        const TEST: &str = "processes_a_crashed_client_leaves_behind_end_with_it";
        if supervised_child() {
            let leftover = unsafe { libc::fork() };
            if leftover == 0 {
                loop {
                    unsafe { libc::pause() };
                }
            }
            write_raw(
                RawStream::Stdout,
                format!("leftover {leftover}\n").as_bytes(),
            );
            die_by(libc::SIGABRT);
        }

        let run = supervise(TEST);
        let leftover: libc::pid_t = String::from_utf8_lossy(&run.stdout)
            .lines()
            .find_map(|line| line.strip_prefix("leftover ")?.parse().ok())
            .expect("the client names the process it left behind");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !ended(leftover) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let gone = ended(leftover);
        if !gone {
            unsafe { libc::kill(leftover, libc::SIGKILL) };
        }
        assert_eq!(run.finished.end, RunEnd::Crashed(Signal(libc::SIGABRT)));
        assert!(gone, "a process the crashed client left behind outlived it");
    }

    #[test]
    fn a_quiet_exit_without_a_record_is_the_client_closing_itself() {
        if supervised_child() {
            unsafe { libc::_exit(0) }
        }

        let run = supervise("a_quiet_exit_without_a_record_is_the_client_closing_itself");
        assert_eq!(run.finished.end, RunEnd::ClosedItself);
        assert_eq!(run.finished.failure(), None);
        assert!(
            footer(&run.log)[0].ends_with("  INFO eclipse::supervisor: Roblox closed itself"),
            "{}",
            run.log
        );
    }

    #[test]
    fn a_client_exit_record_keeps_the_status_roblox_asked_for() {
        if supervised_child() {
            finish_android_process(ClientEnd::ClientExited { status: 10 });
        }

        let run = supervise("a_client_exit_record_keeps_the_status_roblox_asked_for");
        assert_eq!(run.finished.end, RunEnd::ExitedUnexpectedly { status: 10 });
        assert_eq!(run.finished.exit_code(), ExitCode::from(10));
        assert!(!run.log.contains("ignored an exit record"), "{}", run.log);
    }

    #[test]
    fn an_uncaught_java_exception_before_a_clean_exit_is_unexpected() {
        if supervised_child() {
            write_raw(
                RawStream::Stderr,
                b"Exception in thread \"main\" java.lang.RuntimeException: boom\n\
                  \tat com.roblox.client.Main.run(Main.java:1)\n",
            );
            unsafe { libc::_exit(0) }
        }

        let run = supervise("an_uncaught_java_exception_before_a_clean_exit_is_unexpected");
        assert_eq!(run.finished.end, RunEnd::ExitedUnexpectedly { status: 0 });
        assert_eq!(run.finished.exit_code(), ExitCode::SUCCESS);
    }

    #[test]
    fn a_terminated_client_was_stopped_and_a_killed_one_is_shown() {
        if supervised_child() {
            die_by(match std::env::var("ECLIPSE_TEST_SIGNAL").as_deref() {
                Ok("TERM") => libc::SIGTERM,
                _ => libc::SIGKILL,
            });
        }

        for (signal, end, shown) in [
            ("TERM", RunEnd::Stopped(Signal(libc::SIGTERM)), false),
            (
                "KILL",
                RunEnd::Killed {
                    oom_kills: oom_kills().map(|_| 0),
                },
                true,
            ),
        ] {
            let dir = temp_dir(&format!("signal-{signal}"));
            let mut command =
                child_command("a_terminated_client_was_stopped_and_a_killed_one_is_shown");
            command.env("ECLIPSE_TEST_SIGNAL", signal);
            let output = Output {
                stdout: Vec::new(),
                stderr: Vec::new(),
                echo_records: false,
            };
            let finished = run(
                command,
                lock_in(&dir),
                &dir,
                RunLog::start(&dir).unwrap(),
                output,
            );
            std::fs::remove_dir_all(&dir).ok();
            let finished = finished.unwrap();
            assert_eq!(finished.end, end, "{signal}");
            assert_eq!(finished.failure().is_some(), shown, "{signal}");
        }
    }

    #[test]
    fn records_are_echoed_to_a_terminal_except_status_records() {
        if supervised_child() {
            tracing::warn!(target: "liblog", "first line\ncontinued line");
            eclipse::diagnostics::record_status(Level::WARN, "printed by the status sink");
            finish_android_process(ClientEnd::Played);
        }

        let dir = temp_dir("echo");
        let mut stderr = Vec::new();
        let output = Output {
            stdout: io::sink(),
            stderr: &mut stderr,
            echo_records: true,
        };
        let command = child_command("records_are_echoed_to_a_terminal_except_status_records");
        let finished = run(
            command,
            lock_in(&dir),
            &dir,
            RunLog::start(&dir).unwrap(),
            output,
        );
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(finished.unwrap().end, RunEnd::Ended);
        let stderr = String::from_utf8(stderr).unwrap();
        assert!(
            stderr.contains("  WARN liblog: first line\ncontinued line\n"),
            "{stderr}"
        );
        assert!(!stderr.contains("printed by the status sink"), "{stderr}");
    }

    #[test]
    fn join_codes_in_client_records_reach_neither_the_log_nor_the_terminal() {
        const ACCESS: &str = "8f3c2a10-5b6d-4e7f-9a1b-2c3d4e5f6a7b";
        const LINK: &str = "linkCode=123";
        const SHARE: &str = "2f4c6e8a0b1d3f5a7c9e1b3d5f7a9c0e";
        if supervised_child() {
            tracing::info!(
                target: "android.util.Log",
                tag = "ContextImpl",
                "startActivity(Intent {{ uri=roblox://placeId=1818&accessCode={ACCESS}&{LINK} }}) \
                 called"
            );
            tracing::warn!(
                target: "liblog",
                "Share link received: https://www.roblox.com/share?code={SHARE}&type=Server"
            );
            eclipse::diagnostics::record_status(
                Level::WARN,
                &format!("joining with accessCode={ACCESS}"),
            );
            finish_android_process(ClientEnd::Played);
        }

        let dir = temp_dir("join-codes");
        let mut stderr = Vec::new();
        let output = Output {
            stdout: io::sink(),
            stderr: &mut stderr,
            echo_records: true,
        };
        let command =
            child_command("join_codes_in_client_records_reach_neither_the_log_nor_the_terminal");
        let finished = run(
            command,
            lock_in(&dir),
            &dir,
            RunLog::start(&dir).unwrap(),
            output,
        );
        let log = finished
            .as_ref()
            .ok()
            .map(|finished| std::fs::read_to_string(&finished.log));
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(finished.unwrap().end, RunEnd::Ended);
        let log = log.unwrap().unwrap();
        let stderr = String::from_utf8(stderr).unwrap();
        for text in [&log, &stderr] {
            for secret in [ACCESS, LINK, SHARE] {
                assert!(!text.contains(secret), "{secret} in\n{text}");
            }
        }
        for redacted in [
            "uri=roblox://placeId=1818&accessCode=<redacted>&linkCode=<redacted> }",
            "https://www.roblox.com/share?code=<redacted>&type=Server",
        ] {
            assert!(log.contains(redacted), "{redacted}\n{log}");
            assert!(stderr.contains(redacted), "{redacted}\n{stderr}");
        }
        assert!(
            log.contains("  WARN eclipse::status: joining with accessCode=<redacted>\n"),
            "{log}"
        );
    }

    static SIGTERM_MARKER: AtomicI32 = AtomicI32::new(-1);

    extern "C" fn record_sigterm(_: libc::c_int) {
        const TEXT: &[u8] = b"SIGTERM\n";
        unsafe {
            libc::write(
                SIGTERM_MARKER.load(Ordering::Relaxed),
                TEXT.as_ptr().cast(),
                TEXT.len(),
            );
            libc::_exit(0);
        }
    }

    #[test]
    fn a_client_gets_sigterm_within_two_seconds_of_its_supervisor_dying() {
        const TEST: &str = "a_client_gets_sigterm_within_two_seconds_of_its_supervisor_dying";
        const MARKER: &str = "ECLIPSE_TEST_SIGTERM_MARKER";
        if supervised_child() {
            let marker = File::create(std::env::var_os(MARKER).unwrap()).unwrap();
            SIGTERM_MARKER.store(
                std::os::fd::IntoRawFd::into_raw_fd(marker),
                Ordering::Relaxed,
            );
            let handler: extern "C" fn(libc::c_int) = record_sigterm;
            unsafe { libc::signal(libc::SIGTERM, handler as libc::sighandler_t) };
            write_raw(
                RawStream::Stdout,
                format!("ready {}\n", std::process::id()).as_bytes(),
            );
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        if let Some(dir) = std::env::var_os(PARENT) {
            let dir = PathBuf::from(dir);
            let mut command = child_command(TEST);
            command.env(MARKER, dir.join("marker"));
            let output = Output {
                stdout: io::stdout(),
                stderr: io::stderr(),
                echo_records: false,
            };
            let finished = run(
                command,
                lock_in(&dir),
                &dir,
                RunLog::start(&dir).unwrap(),
                output,
            );
            panic!(
                "the supervisor outlived the test: {:?}",
                finished.map(|run| run.end)
            );
        }

        let dir = temp_dir("parent-death");
        let mut parent = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("supervisor::tests::{TEST}")])
            .env(PARENT, &dir)
            .env_remove(CHILD)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = io::BufReader::new(parent.stdout.take().unwrap());
        let (ready, started) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in io::BufRead::lines(stdout).map_while(Result::ok) {
                if let Some(pid) = line.strip_prefix("ready ") {
                    ready.send(pid.parse::<libc::pid_t>().unwrap()).ok();
                }
            }
        });
        let child = started.recv_timeout(Duration::from_secs(60));
        parent.kill().unwrap();
        parent.wait().unwrap();
        let child = child.expect("the supervised child starts");
        let deadline = Instant::now() + Duration::from_secs(2);
        let marker = dir.join("marker");
        let terminated = loop {
            if std::fs::read(&marker).is_ok_and(|text| text == b"SIGTERM\n") {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        if !terminated {
            unsafe { libc::kill(child, libc::SIGKILL) };
        }
        std::fs::remove_dir_all(&dir).ok();
        assert!(
            terminated,
            "the client outlived its supervisor by more than 2 s"
        );
    }

    fn flood_error(record: usize) -> bool {
        record.is_multiple_of(100)
    }

    fn flood_message(record: usize) -> String {
        let padding = if record % 10_000 == 1_234 {
            FLOOD_LONG_BYTES
        } else {
            0
        };
        format!("flood {record:06} {}", "x".repeat(padding))
    }

    #[test]
    fn a_flood_from_four_threads_arrives_whole_and_complete() {
        if supervised_child() {
            let per_thread = FLOOD_RECORDS / FLOOD_THREADS;
            std::thread::scope(|scope| {
                for thread in 0..FLOOD_THREADS {
                    scope.spawn(move || {
                        for record in thread * per_thread..(thread + 1) * per_thread {
                            let message = flood_message(record);
                            if flood_error(record) {
                                tracing::error!(target: "flood", "{message}");
                            } else {
                                tracing::info!(target: "flood", "{message}");
                            }
                        }
                    });
                }
            });
            finish_android_process(ClientEnd::Played);
        }

        let run = supervise("a_flood_from_four_threads_arrives_whole_and_complete");
        assert_eq!(run.finished.end, RunEnd::Ended);
        assert!(
            run.finished.log.to_str().unwrap().ends_with(".tail.log"),
            "{}",
            run.finished.log.display()
        );
        assert!(
            run.newest_part
                .ends_with("  INFO eclipse::supervisor: Roblox ended without an error\n"),
            "the outcome names the part that holds the run's last line"
        );
        let mut seen = vec![false; FLOOD_RECORDS];
        let (mut errors, mut long) = (0, 0);
        for line in run.log.lines() {
            let Some(head) = record_head(line) else {
                panic!("a broken record: {line:.200}");
            };
            if head.target != "flood" {
                continue;
            }
            let record: usize = head.message[6..12].parse().unwrap();
            assert_eq!(head.message, flood_message(record));
            assert_eq!(head.level == Level::ERROR, flood_error(record), "{record}");
            assert!(!seen[record], "{record} arrived twice");
            seen[record] = true;
            errors += usize::from(head.level == Level::ERROR);
            long += usize::from(line.len() > PIPE_BUF);
        }
        assert!(seen.iter().all(|seen| *seen));
        assert_eq!((errors, long), (1_000, 10));
    }

    #[test]
    fn every_exit_record_and_wait_status_has_one_outcome() {
        let killed = exit_status(libc::SIGKILL);
        let segv = exit_status(libc::SIGSEGV);
        for (record, status, uncaught, oom_kills, end) in [
            (Some(ClientEnd::Played), segv, true, None, RunEnd::Ended),
            (
                Some(ClientEnd::WindowClosed),
                exited(1),
                false,
                None,
                RunEnd::Ended,
            ),
            (
                Some(ClientEnd::ClosedForAnotherLaunch),
                exited(0),
                false,
                None,
                RunEnd::Ended,
            ),
            (
                Some(ClientEnd::FailureShown),
                exited(1),
                false,
                None,
                RunEnd::FailureShown,
            ),
            (
                Some(ClientEnd::ClientExited { status: 0 }),
                exited(0),
                false,
                None,
                RunEnd::ClosedItself,
            ),
            (
                Some(ClientEnd::ClientExited { status: 0 }),
                exited(0),
                true,
                None,
                RunEnd::ExitedUnexpectedly { status: 0 },
            ),
            (
                Some(ClientEnd::ClientExited { status: 10 }),
                exited(10),
                false,
                None,
                RunEnd::ExitedUnexpectedly { status: 10 },
            ),
            (
                None,
                exit_status(libc::SIGINT),
                false,
                None,
                RunEnd::Stopped(Signal(libc::SIGINT)),
            ),
            (
                None,
                exit_status(libc::SIGTERM),
                true,
                None,
                RunEnd::Stopped(Signal(libc::SIGTERM)),
            ),
            (
                None,
                exit_status(libc::SIGHUP),
                false,
                None,
                RunEnd::Stopped(Signal(libc::SIGHUP)),
            ),
            (
                None,
                killed,
                false,
                Some(1),
                RunEnd::Killed { oom_kills: Some(1) },
            ),
            (
                None,
                killed,
                false,
                None,
                RunEnd::Killed { oom_kills: None },
            ),
            (
                None,
                segv,
                false,
                None,
                RunEnd::Crashed(Signal(libc::SIGSEGV)),
            ),
            (
                None,
                exit_status(libc::SIGABRT | 0x80),
                false,
                None,
                RunEnd::Crashed(Signal(libc::SIGABRT)),
            ),
            (None, exited(0), false, None, RunEnd::ClosedItself),
            (
                None,
                exited(0),
                true,
                None,
                RunEnd::ExitedUnexpectedly { status: 0 },
            ),
            (
                None,
                exited(1),
                false,
                None,
                RunEnd::ExitedUnexpectedly { status: 1 },
            ),
        ] {
            assert_eq!(
                classify(record, status, uncaught, None, oom_kills),
                end,
                "{record:?} {status:?} {uncaught}"
            );
        }
    }

    #[test]
    fn an_art_fatal_signal_report_turns_only_a_plain_exit_into_a_crash() {
        let abort = Some(Signal(libc::SIGABRT));
        for (record, status, end) in [
            (None, exited(1), RunEnd::Crashed(Signal(libc::SIGABRT))),
            (
                None,
                exit_status(libc::SIGSEGV),
                RunEnd::Crashed(Signal(libc::SIGSEGV)),
            ),
            (
                Some(ClientEnd::FailureShown),
                exited(1),
                RunEnd::FailureShown,
            ),
        ] {
            assert_eq!(
                classify(record, status, false, abort, None),
                end,
                "{record:?} {status:?}"
            );
        }
    }

    #[test]
    fn linkage_errors_name_only_missing_android_apis() {
        for (line, api) in [
            (
                "java.lang.NoSuchFieldError: No field EFFECT_TYPE_AEC of type Ljava/util/UUID; in \
                 class Landroid/media/audiofx/AudioEffect; or its superclasses (declaration of \
                 'android.media.audiofx.AudioEffect' appears in \
                 /app/lib/eclipse/framework/api-impl.jar!classes3.dex)",
                Some("android.media.audiofx.AudioEffect.EFFECT_TYPE_AEC"),
            ),
            (
                "Exception in thread \"Thread-17\" java.lang.NoSuchMethodError: No virtual method \
                 isDeviceSecure()Z in class Landroid/app/KeyguardManager; or its super classes",
                Some("android.app.KeyguardManager.isDeviceSecure()Z"),
            ),
            (
                "Caused by: java.lang.NoClassDefFoundError: Failed resolution of: \
                 Landroid/media/MediaExtractor;",
                Some("android.media.MediaExtractor"),
            ),
            (
                "java.lang.NoSuchMethodError: no static method \
                 \"Landroid/os/Build;.getSerial()Ljava/lang/String;\"",
                Some("android.os.Build.getSerial()Ljava/lang/String;"),
            ),
            (
                "Exception in thread \"main\" java.lang.NoSuchFieldError: no \
                 \"Ljava/lang/String;\" field \"SOC_MODEL\" in class \"Landroid/os/Build;\" or \
                 its superclasses",
                Some("android.os.Build.SOC_MODEL"),
            ),
            (
                "java.lang.NoSuchFieldError: no type \"Landroid/media/AudioDeviceInfo;\" found and \
                 so no field \"device\" could be found in class \"Lcom/roblox/b;\" or its \
                 superclasses",
                Some("android.media.AudioDeviceInfo"),
            ),
            (
                "java.lang.NoSuchFieldError: No static field a of type I in class Lcom/roblox/b; \
                 or its superclasses",
                None,
            ),
            (
                "java.lang.NoSuchMethodError: no non-static method \"Lcom/roblox/b;.a()V\"",
                None,
            ),
            (
                "java.lang.NoSuchFieldError: no \"I\" field \"a\" in class \"Lcom/roblox/b;\" or \
                 its superclasses",
                None,
            ),
            (
                "java.lang.NoSuchFieldException: EFFECT_TYPE_AEC in \
                 Landroid/media/audiofx/AudioEffect;",
                None,
            ),
            (
                "\tat org.webrtc.voiceengine.WebRtcAudioManager.<init>(Unknown Source:81)",
                None,
            ),
        ] {
            assert_eq!(
                missing_android_api(line.as_bytes()).as_deref(),
                api,
                "{line}"
            );
        }
    }

    #[test]
    fn an_art_fatal_signal_before_a_status_exit_is_a_crash_naming_the_missing_android_api() {
        if supervised_child() {
            write_raw(
                RawStream::Stderr,
                b"java.lang.NoSuchFieldError: No field EFFECT_TYPE_AEC of type Ljava/util/UUID; \
                  in class Landroid/media/audiofx/AudioEffect; or its superclasses (declaration \
                  of 'android.media.audiofx.AudioEffect' appears in \
                  /app/lib/eclipse/framework/api-impl.jar!classes3.dex)\n\
                  \tat org.webrtc.voiceengine.WebRtcAudioManager.<init>(Unknown Source:81)\n\
                  # Check failed: !jni_->ExceptionCheck()\n\
                  *** *** *** *** *** *** *** *** *** *** *** *** *** *** *** ***\n\
                  Fatal signal 6 (SIGABRT), code -6 (SI_TKILL)\n\
                  A/art     (    3): art/runtime/runtime_common.cc:458] HandleUnexpectedSignal \
                  reenter\n",
            );
            unsafe { libc::_exit(1) }
        }

        let run = supervise(
            "an_art_fatal_signal_before_a_status_exit_is_a_crash_naming_the_missing_android_api",
        );
        assert_eq!(run.finished.end, RunEnd::Crashed(Signal(libc::SIGABRT)));
        assert_eq!(run.finished.exit_code(), ExitCode::FAILURE);
        assert_eq!(
            run.finished.failure().as_deref(),
            Some(
                "Roblox crashed (signal 6, SIGABRT: Roblox aborted)\nMissing Android API: \
                 android.media.audiofx.AudioEffect.EFFECT_TYPE_AEC"
            )
        );
        assert_eq!(footer(&run.log).len(), 2, "{}", run.log);
    }

    #[test]
    fn a_linkage_error_after_the_fatal_signal_report_is_not_the_crash_detail() {
        if supervised_child() {
            write_raw(
                RawStream::Stderr,
                b"Fatal signal 11 (SIGSEGV), code 1 (SEGV_MAPERR)\n\
                  java.lang.NoClassDefFoundError: Failed resolution of: \
                  Landroid/media/MediaExtractor;\n",
            );
            unsafe { libc::_exit(1) }
        }

        let run =
            supervise("a_linkage_error_after_the_fatal_signal_report_is_not_the_crash_detail");
        assert_eq!(
            run.finished.failure().as_deref(),
            Some("Roblox crashed (signal 11, SIGSEGV: invalid memory access)")
        );
    }

    #[test]
    fn outcomes_are_named_in_plain_words() {
        for (end, text, shown) in [
            (RunEnd::Ended, "Roblox ended without an error", false),
            (
                RunEnd::FailureShown,
                "Roblox stopped after Eclipse reported why",
                false,
            ),
            (
                RunEnd::Stopped(Signal(libc::SIGTERM)),
                "Roblox was stopped (signal 15, SIGTERM)",
                false,
            ),
            (
                RunEnd::Killed { oom_kills: Some(2) },
                "Roblox was killed (SIGKILL); the kernel's out-of-memory killer stopped it",
                true,
            ),
            (
                RunEnd::Killed { oom_kills: Some(0) },
                "Roblox was killed (SIGKILL)",
                true,
            ),
            (
                RunEnd::Killed { oom_kills: None },
                "Roblox was killed (SIGKILL); this is often the out-of-memory killer",
                true,
            ),
            (
                RunEnd::Crashed(Signal(libc::SIGILL)),
                "Roblox crashed (signal 4, SIGILL: an illegal instruction or a deliberate trap \
                 in Roblox's code)",
                true,
            ),
            (
                RunEnd::Crashed(Signal(libc::SIGABRT)),
                "Roblox crashed (signal 6, SIGABRT: Roblox aborted)",
                true,
            ),
            (
                RunEnd::Crashed(Signal(42)),
                "Roblox crashed (signal 42)",
                true,
            ),
            (RunEnd::ClosedItself, "Roblox closed itself", false),
            (
                RunEnd::ExitedUnexpectedly { status: 3 },
                "Roblox exited unexpectedly with status 3",
                true,
            ),
        ] {
            assert_eq!(end.to_string(), text);
            assert_eq!(end.shown(), shown, "{text}");
        }
    }

    #[test]
    fn only_a_pipe_beyond_the_standard_streams_is_adopted() {
        use std::os::fd::IntoRawFd as _;

        let (reader, _writer) = io::pipe().unwrap();
        rustix::io::fcntl_setfd(&reader, FdFlags::empty()).unwrap();
        let pipe = adopt_pipe(reader.into_raw_fd()).unwrap();
        assert!(rustix::io::fcntl_getfd(&pipe)
            .unwrap()
            .contains(FdFlags::CLOEXEC));

        let file = File::open("/dev/null").unwrap();
        let error = adopt_pipe(file.as_raw_fd()).unwrap_err();
        assert!(error.contains("not a pipe"), "{error}");
        for fd in [libc::STDIN_FILENO, libc::STDERR_FILENO] {
            let error = adopt_pipe(fd).unwrap_err();
            assert!(error.contains("standard stream"), "{error}");
        }
        drop(file);
    }

    #[test]
    fn lines_split_across_reads_are_joined_and_long_ones_are_cut() {
        let mut lines = Lines::default();
        let mut seen = Vec::new();
        lines.feed(b"first\nsec", |line| seen.push(line.to_vec()));
        lines.feed(b"ond\n\nthi", |line| seen.push(line.to_vec()));
        lines.feed(&vec![b'x'; RAW_LINE_BYTES], |line| {
            seen.push(line.len().to_string().into_bytes())
        });
        lines.feed(b"tail", |line| seen.push(line.to_vec()));
        lines.finish(|line| seen.push(line.to_vec()));
        assert_eq!(
            seen,
            [
                b"first".to_vec(),
                b"second".to_vec(),
                Vec::new(),
                (RAW_LINE_BYTES + 3).to_string().into_bytes(),
                b"tail".to_vec(),
            ]
        );
    }

    #[test]
    fn oom_kills_come_from_the_unified_cgroup_memory_events() {
        assert_eq!(
            unified_cgroup("1:name=systemd:/legacy\n0::/user.slice/app-flatpak.scope\n"),
            Some("/user.slice/app-flatpak.scope")
        );
        assert_eq!(unified_cgroup("12:pids:/user.slice\n"), None);
        assert_eq!(
            oom_kill_count("low 0\nhigh 0\nmax 3\noom 2\noom_kill 1\noom_group_kill 0\n"),
            Some(1)
        );
        assert_eq!(oom_kill_count("oom 2\noom_group_kill 4\n"), None);
    }
}
