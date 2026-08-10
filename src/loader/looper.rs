use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;

pub const ALOOPER_POLL_WAKE: i32 = -1;

pub const ALOOPER_POLL_CALLBACK: i32 = -2;

pub const ALOOPER_POLL_TIMEOUT: i32 = -3;

pub const ALOOPER_POLL_ERROR: i32 = -4;

pub const ALOOPER_EVENT_INPUT: i32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PollFd {
    fd: i32,
    ident: i32,
    events: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollResult {
    Fd { ident: i32, fd: i32, events: i32 },

    Wake,

    Timeout,

    Error,
}

#[derive(Debug)]
pub struct Looper {
    wake_fd: Arc<OwnedFd>,

    fds: Vec<PollFd>,
}

impl Looper {
    pub fn new() -> Option<Self> {
        let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if raw < 0 {
            return None;
        }

        let wake_fd = unsafe { owned_from_raw(raw) };
        Some(Self {
            wake_fd: Arc::new(wake_fd),
            fds: Vec::new(),
        })
    }

    pub fn add_fd(&mut self, fd: i32, ident: i32, events: i32) {
        if let Some(existing) = self.fds.iter_mut().find(|p| p.fd == fd) {
            existing.ident = ident;
            existing.events = events;
        } else {
            self.fds.push(PollFd { fd, ident, events });
        }
    }

    pub fn remove_fd(&mut self, fd: i32) -> bool {
        let before = self.fds.len();
        self.fds.retain(|p| p.fd != fd);
        self.fds.len() != before
    }

    pub fn waker(&self) -> Waker {
        Waker {
            wake_fd: Arc::clone(&self.wake_fd),
        }
    }

    pub fn snapshot(&self) -> PollSnapshot {
        PollSnapshot {
            wake_fd: Arc::clone(&self.wake_fd),
            fds: self.fds.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Waker {
    wake_fd: Arc<OwnedFd>,
}

impl Waker {
    pub fn wake(&self) {
        write_wake(self.wake_fd.as_raw_fd());
    }
}

#[derive(Debug)]
pub struct PollSnapshot {
    wake_fd: Arc<OwnedFd>,
    fds: Vec<PollFd>,
}

impl PollSnapshot {
    pub fn poll_once(&self, timeout_millis: i32) -> PollResult {
        let mut pfds: Vec<libc::pollfd> = Vec::with_capacity(self.fds.len() + 1);
        pfds.push(libc::pollfd {
            fd: self.wake_fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        for p in &self.fds {
            pfds.push(libc::pollfd {
                fd: p.fd,

                events: (p.events & i32::from(u16::MAX)) as libc::c_short,
                revents: 0,
            });
        }

        let rc = unsafe {
            libc::poll(
                pfds.as_mut_ptr(),
                pfds.len() as libc::nfds_t,
                timeout_millis,
            )
        };

        if rc < 0 {
            return if last_errno() == libc::EINTR {
                PollResult::Wake
            } else {
                PollResult::Error
            };
        }
        if rc == 0 {
            return PollResult::Timeout;
        }

        if pfds[0].revents & libc::POLLIN != 0 {
            drain_eventfd(self.wake_fd.as_raw_fd());
            return PollResult::Wake;
        }

        for (slot, p) in pfds[1..].iter().zip(self.fds.iter()) {
            if slot.revents != 0 {
                return PollResult::Fd {
                    ident: p.ident,
                    fd: p.fd,
                    events: i32::from(slot.revents),
                };
            }
        }

        PollResult::Wake
    }
}

fn write_wake(fd: i32) {
    let one: u64 = 1;

    let _ = unsafe {
        libc::write(
            fd,
            std::ptr::addr_of!(one).cast(),
            std::mem::size_of::<u64>(),
        )
    };
}

fn drain_eventfd(fd: i32) {
    let mut buf: u64 = 0;

    let _ = unsafe {
        libc::read(
            fd,
            std::ptr::addr_of_mut!(buf).cast(),
            std::mem::size_of::<u64>(),
        )
    };
}

unsafe fn owned_from_raw(raw: i32) -> OwnedFd {
    use std::os::fd::FromRawFd;

    unsafe { OwnedFd::from_raw_fd(raw) }
}

fn last_errno() -> i32 {
    unsafe { *libc::__errno_location() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::time::Instant;

    struct TestPipe {
        read: OwnedFd,
        write: std::fs::File,
    }

    impl TestPipe {
        fn new() -> Self {
            let mut fds = [0i32; 2];

            let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
            assert_eq!(rc, 0, "pipe2 failed");

            let read = unsafe { owned_from_raw(fds[0]) };
            let write = unsafe {
                use std::os::fd::FromRawFd;
                std::fs::File::from_raw_fd(fds[1])
            };
            Self { read, write }
        }
        fn read_fd(&self) -> i32 {
            self.read.as_raw_fd()
        }
        fn signal(&mut self) {
            self.write.write_all(b"x").expect("write to pipe");
        }
    }

    #[test]
    fn new_looper_polls_out_timeout_with_no_source() {
        let looper = Looper::new().expect("eventfd");

        let start = Instant::now();
        assert_eq!(looper.snapshot().poll_once(10), PollResult::Timeout);
        assert!(
            start.elapsed().as_millis() < 2000,
            "poll honored the timeout"
        );
    }

    #[test]
    fn wake_unblocks_poll_and_returns_wake() {
        let looper = Looper::new().expect("eventfd");

        looper.waker().wake();
        assert_eq!(looper.snapshot().poll_once(0), PollResult::Wake);

        assert_eq!(looper.snapshot().poll_once(0), PollResult::Timeout);
    }

    #[test]
    fn wake_from_another_thread_unblocks_a_parked_poll() {
        let looper = Looper::new().expect("eventfd");
        let waker = looper.waker();

        let h = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            waker.wake();
        });
        let snap = looper.snapshot();
        let start = Instant::now();

        assert_eq!(snap.poll_once(-1), PollResult::Wake);
        assert!(
            start.elapsed().as_millis() >= 40,
            "poll actually blocked until the wake"
        );
        h.join().expect("waker thread");
    }

    #[test]
    fn registered_fd_ready_returns_its_ident() {
        let mut looper = Looper::new().expect("eventfd");
        let mut pipe = TestPipe::new();
        const ENGINE_INPUT_IDENT: i32 = 7;
        looper.add_fd(pipe.read_fd(), ENGINE_INPUT_IDENT, ALOOPER_EVENT_INPUT);

        assert_eq!(looper.snapshot().poll_once(10), PollResult::Timeout);

        pipe.signal();
        match looper.snapshot().poll_once(100) {
            PollResult::Fd { ident, fd, events } => {
                assert_eq!(ident, ENGINE_INPUT_IDENT, "returns the registered ident");
                assert_eq!(fd, pipe.read_fd(), "reports the fd that fired");
                assert!(events & ALOOPER_EVENT_INPUT != 0, "reports POLLIN");
            }
            other => panic!("expected Fd, got {other:?}"),
        }
    }

    #[test]
    fn wake_takes_priority_over_a_ready_fd() {
        let mut looper = Looper::new().expect("eventfd");
        let mut pipe = TestPipe::new();
        looper.add_fd(pipe.read_fd(), 3, ALOOPER_EVENT_INPUT);
        pipe.signal();
        looper.waker().wake();
        assert_eq!(looper.snapshot().poll_once(100), PollResult::Wake);

        match looper.snapshot().poll_once(100) {
            PollResult::Fd { ident, .. } => assert_eq!(ident, 3),
            other => panic!("expected Fd after wake drained, got {other:?}"),
        }
    }

    #[test]
    fn remove_fd_stops_it_from_firing() {
        let mut looper = Looper::new().expect("eventfd");
        let mut pipe = TestPipe::new();
        looper.add_fd(pipe.read_fd(), 9, ALOOPER_EVENT_INPUT);
        assert!(looper.remove_fd(pipe.read_fd()), "fd was present");
        assert!(!looper.remove_fd(pipe.read_fd()), "now absent");
        pipe.signal();

        assert_eq!(looper.snapshot().poll_once(10), PollResult::Timeout);
    }

    #[test]
    fn add_fd_twice_replaces_not_duplicates() {
        let mut looper = Looper::new().expect("eventfd");
        let pipe = TestPipe::new();
        looper.add_fd(pipe.read_fd(), 1, ALOOPER_EVENT_INPUT);
        looper.add_fd(pipe.read_fd(), 2, ALOOPER_EVENT_INPUT);
        assert_eq!(looper.fds.len(), 1, "re-add replaces the registration");
        assert_eq!(looper.fds[0].ident, 2, "ident updated to the latest");
    }
}
