use std::fs::File;
use std::io::{self, Read as _, Seek as _};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::io::Errno;
use rustix::process::{Pid, PidfdFlags};

pub(crate) enum Bounded {
    Finished(Output),
    TimedOut(Output),
}

pub(crate) fn run(command: &mut Command, limit: Duration) -> io::Result<Bounded> {
    let stdout = capture("stdout")?;
    let stderr = capture("stderr")?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?)
        .spawn()?;
    let exited = match wait(&mut child, limit) {
        Ok(exited) => exited,
        Err(error) => {
            stop(&mut child)?;
            return Err(error);
        }
    };
    let output = |status| -> io::Result<Output> {
        Ok(Output {
            status,
            stdout: contents(stdout)?,
            stderr: contents(stderr)?,
        })
    };
    match exited {
        Some(status) => output(status).map(Bounded::Finished),
        None => output(stop(&mut child)?).map(Bounded::TimedOut),
    }
}

fn wait(child: &mut Child, limit: Duration) -> io::Result<Option<ExitStatus>> {
    let exit = rustix::process::pidfd_open(Pid::from_child(child), PidfdFlags::empty())?;
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        let timeout = Timespec::try_from(remaining)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        match rustix::event::poll(&mut [PollFd::new(&exit, PollFlags::IN)], Some(&timeout)) {
            Ok(_) | Err(Errno::INTR) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

fn stop(child: &mut Child) -> io::Result<ExitStatus> {
    child.kill()?;
    child.wait()
}

#[cfg(test)]
pub(crate) fn output(command: &mut Command, limit: Duration) -> Output {
    match run(command, limit) {
        Ok(Bounded::Finished(output)) => output,
        Ok(Bounded::TimedOut(output)) => panic!(
            "{command:?} was still running after {limit:?}, so it was killed\n\
             stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
        Err(error) => panic!("cannot run {command:?} to completion: {error}"),
    }
}

fn capture(stream: &str) -> io::Result<File> {
    rustix::fs::memfd_create(stream, rustix::fs::MemfdFlags::CLOEXEC)
        .map(File::from)
        .map_err(io::Error::from)
}

fn contents(mut capture: File) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    capture.rewind()?;
    capture.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;

    use super::*;

    #[test]
    fn a_child_that_exits_is_collected_with_its_output() {
        let started = Instant::now();
        let finished = run(
            Command::new("sh").args(["-c", "sleep 0.2; echo out; echo err >&2; exit 3"]),
            Duration::from_secs(30),
        );
        let elapsed = started.elapsed();
        let Ok(Bounded::Finished(output)) = finished else {
            panic!("the child did not finish");
        };
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(output.stdout, b"out\n");
        assert_eq!(output.stderr, b"err\n");
        assert!(elapsed >= Duration::from_millis(200), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(30), "{elapsed:?}");
    }

    #[test]
    fn a_child_still_running_at_the_limit_is_killed() {
        let started = Instant::now();
        let ran = run(
            Command::new("sh").args(["-c", "echo started; exec sleep 30"]),
            Duration::from_millis(300),
        );
        let elapsed = started.elapsed();
        let Ok(Bounded::TimedOut(output)) = ran else {
            panic!("the child was not stopped at the limit");
        };
        assert_eq!(output.status.signal(), Some(libc::SIGKILL));
        assert_eq!(output.stdout, b"started\n");
        assert!(elapsed >= Duration::from_millis(300), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(30), "{elapsed:?}");
    }
}
