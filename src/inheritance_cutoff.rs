use std::ffi::{c_int, c_uint};
use std::io;
use std::os::fd::RawFd;

use rustix::process::{getrlimit, Resource};

#[derive(Debug, Clone, Copy)]
pub(crate) struct InheritanceCutoff {
    first: c_uint,
    open_limit: c_uint,
}

impl InheritanceCutoff {
    pub(crate) fn at(first: RawFd) -> Self {
        let open_limit = getrlimit(Resource::Nofile)
            .current
            .map_or(c_uint::MAX, |limit| {
                c_uint::try_from(limit).unwrap_or(c_uint::MAX)
            });
        Self {
            first: c_uint::try_from(first).expect("a descriptor number is never negative"),
            open_limit,
        }
    }

    pub(crate) fn mark_close_on_exec(self) -> io::Result<()> {
        let flags = libc::CLOSE_RANGE_CLOEXEC as c_int;
        if unsafe { libc::close_range(self.first, c_uint::MAX, flags) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ENOSYS | libc::EINVAL) => self.mark_each_close_on_exec(),
            _ => Err(error),
        }
    }

    fn mark_each_close_on_exec(self) -> io::Result<()> {
        for fd in self.first..self.open_limit {
            if unsafe { libc::fcntl(fd as c_int, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EBADF) {
                    return Err(error);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
    use std::os::unix::process::CommandExt as _;
    use std::process::Command;

    use super::*;

    fn inheritable_pipe() -> [OwnedFd; 2] {
        let mut ends = [0; 2];
        let piped = unsafe { libc::pipe(ends.as_mut_ptr()) };
        assert_eq!(piped, 0, "pipe: {}", io::Error::last_os_error());
        ends.map(|end| unsafe { OwnedFd::from_raw_fd(end) })
    }

    fn child_inherits_any_of(
        fds: &[OwnedFd],
        before_exec: Option<fn(InheritanceCutoff) -> io::Result<()>>,
    ) -> bool {
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "for fd; do [ -e /proc/self/fd/$fd ] && exit 0; done; exit 1",
            ])
            .arg("sh")
            .args(fds.iter().map(|fd| fd.as_raw_fd().to_string()));
        if let Some(sweep) = before_exec {
            let cutoff = InheritanceCutoff::at(3);
            unsafe {
                command.pre_exec(move || sweep(cutoff));
            }
        }
        command.status().expect("run the stand-in child").success()
    }

    #[test]
    fn a_child_inherits_nothing_at_or_above_the_cutoff() {
        let pipe = inheritable_pipe();
        assert!(
            child_inherits_any_of(&pipe, None),
            "without the cutoff a plain pipe leaks into the child"
        );
        assert!(!child_inherits_any_of(
            &pipe,
            Some(InheritanceCutoff::mark_close_on_exec)
        ));
    }

    #[test]
    fn the_descriptor_by_descriptor_sweep_for_old_kernels_has_the_same_effect() {
        let pipe = inheritable_pipe();
        assert!(!child_inherits_any_of(
            &pipe,
            Some(InheritanceCutoff::mark_each_close_on_exec)
        ));
    }
}
