use std::fs::File;
use std::io::{Read as _, Seek as _};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(20);

pub(crate) fn output(command: &mut Command, limit: Duration) -> Output {
    let stdout = capture("stdout");
    let stderr = capture("stderr");
    let mut child = command
        .stdin(Stdio::null())
        .stdout(share(&stdout))
        .stderr(share(&stderr))
        .spawn()
        .unwrap_or_else(|error| panic!("cannot start {command:?}: {error}"));
    let started = Instant::now();
    loop {
        let exit = child
            .try_wait()
            .unwrap_or_else(|error| panic!("cannot wait for {command:?}: {error}"));
        if let Some(status) = exit {
            return Output {
                status,
                stdout: contents(stdout),
                stderr: contents(stderr),
            };
        }
        if started.elapsed() >= limit {
            child
                .kill()
                .unwrap_or_else(|error| panic!("cannot kill {command:?}: {error}"));
            child
                .wait()
                .unwrap_or_else(|error| panic!("cannot reap {command:?}: {error}"));
            panic!(
                "{command:?} was still running after {limit:?}, so it was killed\n\
                 stdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&contents(stdout)),
                String::from_utf8_lossy(&contents(stderr))
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn capture(stream: &str) -> File {
    rustix::fs::memfd_create(stream, rustix::fs::MemfdFlags::CLOEXEC)
        .map(File::from)
        .unwrap_or_else(|error| panic!("cannot create the {stream} capture: {error}"))
}

fn share(capture: &File) -> File {
    capture
        .try_clone()
        .unwrap_or_else(|error| panic!("cannot hand the capture to the child: {error}"))
}

fn contents(mut capture: File) -> Vec<u8> {
    let mut bytes = Vec::new();
    capture
        .rewind()
        .and_then(|()| capture.read_to_end(&mut bytes))
        .unwrap_or_else(|error| panic!("cannot read the captured output: {error}"));
    bytes
}
