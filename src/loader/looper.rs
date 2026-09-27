use std::ffi::{c_int, c_void};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::Arc;

pub const ALOOPER_POLL_WAKE: i32 = -1;

pub const ALOOPER_POLL_CALLBACK: i32 = -2;

pub const ALOOPER_POLL_TIMEOUT: i32 = -3;

pub const ALOOPER_POLL_ERROR: i32 = -4;

pub const ALOOPER_EVENT_INPUT: i32 = 1;

pub const ALOOPER_EVENT_OUTPUT: i32 = 2;

pub const ALOOPER_EVENT_ERROR: i32 = 4;

pub const ALOOPER_EVENT_HANGUP: i32 = 8;

pub const ALOOPER_EVENT_INVALID: i32 = 16;

const EPOLL_MAX_EVENTS: usize = 16;

pub type AlooperCallback = unsafe extern "C" fn(c_int, c_int, *mut c_void) -> c_int;

#[derive(Debug, Clone, Copy)]
pub enum FdHandler {
    Ident(i32),

    Callback(AlooperCallback),
}

#[derive(Debug, Clone, Copy)]
pub struct Registration {
    pub fd: i32,

    pub handler: FdHandler,

    pub data: usize,

    seq: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct Response {
    pub registration: Registration,

    pub events: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadyFd {
    pub fd: i32,

    pub events: i32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PollOutcome {
    Ready(Vec<ReadyFd>),

    Timeout,

    Error,
}

fn epoll_request(events: i32) -> u32 {
    let mut request = 0;
    if events & ALOOPER_EVENT_INPUT != 0 {
        request |= libc::EPOLLIN as u32;
    }
    if events & ALOOPER_EVENT_OUTPUT != 0 {
        request |= libc::EPOLLOUT as u32;
    }
    request
}

fn alooper_events(epoll_events: u32) -> i32 {
    [
        (libc::EPOLLIN, ALOOPER_EVENT_INPUT),
        (libc::EPOLLOUT, ALOOPER_EVENT_OUTPUT),
        (libc::EPOLLERR, ALOOPER_EVENT_ERROR),
        (libc::EPOLLHUP, ALOOPER_EVENT_HANGUP),
    ]
    .into_iter()
    .filter(|(epoll_bit, _)| epoll_events & *epoll_bit as u32 != 0)
    .fold(0, |events, (_, alooper_bit)| events | alooper_bit)
}

fn epoll_ctl(epoll: &OwnedFd, op: c_int, fd: i32, events: u32) -> io::Result<()> {
    let mut event = libc::epoll_event {
        events,
        u64: fd as u32 as u64,
    };
    if unsafe { libc::epoll_ctl(epoll.as_raw_fd(), op, fd, &mut event) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[derive(Debug)]
pub struct Looper {
    epoll: Arc<OwnedFd>,

    wake_fd: Arc<OwnedFd>,

    registrations: Vec<Registration>,

    next_seq: u64,
}

impl Looper {
    pub fn new() -> io::Result<Self> {
        let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let wake_fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let raw = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let epoll = unsafe { OwnedFd::from_raw_fd(raw) };
        epoll_ctl(
            &epoll,
            libc::EPOLL_CTL_ADD,
            wake_fd.as_raw_fd(),
            libc::EPOLLIN as u32,
        )?;
        Ok(Self {
            epoll: Arc::new(epoll),
            wake_fd: Arc::new(wake_fd),
            registrations: Vec::new(),
            next_seq: 0,
        })
    }

    pub fn add_fd(
        &mut self,
        fd: i32,
        events: i32,
        handler: FdHandler,
        data: usize,
    ) -> io::Result<()> {
        let request = epoll_request(events);
        let existing = self.registrations.iter().position(|r| r.fd == fd);
        match existing {
            Some(_) => match epoll_ctl(&self.epoll, libc::EPOLL_CTL_MOD, fd, request) {
                Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {
                    epoll_ctl(&self.epoll, libc::EPOLL_CTL_ADD, fd, request)
                }
                other => other,
            },
            None => epoll_ctl(&self.epoll, libc::EPOLL_CTL_ADD, fd, request),
        }?;
        let registration = Registration {
            fd,
            handler,
            data,
            seq: self.next_seq,
        };
        self.next_seq += 1;
        match existing {
            Some(index) => self.registrations[index] = registration,
            None => self.registrations.push(registration),
        }
        Ok(())
    }

    pub fn remove_fd(&mut self, fd: i32) -> bool {
        let Some(index) = self.registrations.iter().position(|r| r.fd == fd) else {
            return false;
        };
        self.registrations.swap_remove(index);
        let _ = epoll_ctl(&self.epoll, libc::EPOLL_CTL_DEL, fd, 0);
        true
    }

    pub fn remove_registration(&mut self, registration: &Registration) -> bool {
        let current = self
            .registrations
            .iter()
            .any(|r| r.fd == registration.fd && r.seq == registration.seq);
        current && self.remove_fd(registration.fd)
    }

    pub fn responses(&self, ready: &[ReadyFd]) -> Vec<Response> {
        ready
            .iter()
            .filter_map(|ready| {
                self.registrations
                    .iter()
                    .find(|r| r.fd == ready.fd)
                    .map(|registration| Response {
                        registration: *registration,
                        events: ready.events,
                    })
            })
            .collect()
    }

    pub fn waker(&self) -> Waker {
        Waker {
            wake_fd: Arc::clone(&self.wake_fd),
        }
    }

    pub fn poller(&self) -> Poller {
        Poller {
            epoll: Arc::clone(&self.epoll),
            wake_fd: Arc::clone(&self.wake_fd),
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

#[derive(Debug, Clone)]
pub struct Poller {
    epoll: Arc<OwnedFd>,

    wake_fd: Arc<OwnedFd>,
}

impl Poller {
    pub fn poll(&self, timeout_millis: i32) -> PollOutcome {
        let mut events = [libc::epoll_event { events: 0, u64: 0 }; EPOLL_MAX_EVENTS];
        let count = unsafe {
            libc::epoll_wait(
                self.epoll.as_raw_fd(),
                events.as_mut_ptr(),
                EPOLL_MAX_EVENTS as c_int,
                timeout_millis,
            )
        };
        if count < 0 {
            return if last_errno() == libc::EINTR {
                PollOutcome::Ready(Vec::new())
            } else {
                PollOutcome::Error
            };
        }
        if count == 0 {
            return PollOutcome::Timeout;
        }

        let wake_fd = self.wake_fd.as_raw_fd();
        let mut ready = Vec::with_capacity(count as usize);
        for event in &events[..count as usize] {
            let fd = event.u64 as u32 as i32;
            if fd == wake_fd {
                drain_eventfd(wake_fd);
            } else {
                ready.push(ReadyFd {
                    fd,
                    events: alooper_events(event.events),
                });
            }
        }
        PollOutcome::Ready(ready)
    }

    pub fn readiness_fd(&self) -> BorrowedFd<'_> {
        self.epoll.as_fd()
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

fn last_errno() -> i32 {
    unsafe { *libc::__errno_location() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
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

            let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
            let write = unsafe { std::fs::File::from_raw_fd(fds[1]) };
            Self { read, write }
        }
        fn read_fd(&self) -> i32 {
            self.read.as_raw_fd()
        }
        fn signal(&mut self) {
            self.write.write_all(b"x").expect("write to pipe");
        }
    }

    fn ident_responses(looper: &Looper, timeout_millis: i32) -> Vec<(i32, i32, i32)> {
        match looper.poller().poll(timeout_millis) {
            PollOutcome::Ready(ready) => looper
                .responses(&ready)
                .into_iter()
                .map(|response| match response.registration.handler {
                    FdHandler::Ident(ident) => (ident, response.registration.fd, response.events),
                    FdHandler::Callback(_) => panic!("unexpected callback registration"),
                })
                .collect(),
            other => panic!("expected ready fds, got {other:?}"),
        }
    }

    #[test]
    fn new_looper_polls_out_timeout_with_no_source() {
        let looper = Looper::new().expect("eventfd + epoll");

        let start = Instant::now();
        assert_eq!(looper.poller().poll(10), PollOutcome::Timeout);
        assert!(
            start.elapsed().as_millis() < 2000,
            "poll honored the timeout"
        );
    }

    #[test]
    fn wake_unblocks_poll_and_reports_the_wake() {
        let looper = Looper::new().expect("eventfd + epoll");

        looper.waker().wake();
        assert_eq!(looper.poller().poll(0), PollOutcome::Ready(Vec::new()));

        assert_eq!(looper.poller().poll(0), PollOutcome::Timeout);
    }

    #[test]
    fn wake_from_another_thread_unblocks_a_parked_poll() {
        let looper = Looper::new().expect("eventfd + epoll");
        let waker = looper.waker();

        let h = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            waker.wake();
        });
        let poller = looper.poller();
        let start = Instant::now();

        assert_eq!(poller.poll(-1), PollOutcome::Ready(Vec::new()));
        assert!(
            start.elapsed().as_millis() >= 40,
            "poll actually blocked until the wake"
        );
        h.join().expect("waker thread");
        assert_eq!(poller.poll(0), PollOutcome::Timeout);
    }

    #[test]
    fn registered_fd_ready_reports_its_registration() {
        let mut looper = Looper::new().expect("eventfd + epoll");
        let mut pipe = TestPipe::new();
        const ENGINE_INPUT_IDENT: i32 = 7;
        looper
            .add_fd(
                pipe.read_fd(),
                ALOOPER_EVENT_INPUT,
                FdHandler::Ident(ENGINE_INPUT_IDENT),
                0,
            )
            .expect("add_fd");

        assert_eq!(looper.poller().poll(10), PollOutcome::Timeout);

        pipe.signal();
        assert_eq!(
            ident_responses(&looper, 100),
            vec![(ENGINE_INPUT_IDENT, pipe.read_fd(), ALOOPER_EVENT_INPUT)]
        );
    }

    #[test]
    fn an_fd_added_while_a_poll_is_parked_wakes_that_poll() {
        let mut looper = Looper::new().expect("eventfd + epoll");
        let poller = looper.poller();
        let mut pipe = TestPipe::new();
        pipe.signal();
        let parked = std::thread::spawn(move || poller.poll(5000));
        std::thread::sleep(std::time::Duration::from_millis(50));
        looper
            .add_fd(pipe.read_fd(), ALOOPER_EVENT_INPUT, FdHandler::Ident(1), 0)
            .expect("add_fd");
        match parked.join().expect("parked poll") {
            PollOutcome::Ready(ready) => {
                assert_eq!(
                    ready,
                    vec![ReadyFd {
                        fd: pipe.read_fd(),
                        events: ALOOPER_EVENT_INPUT
                    }]
                );
            }
            other => panic!("expected the new fd, got {other:?}"),
        }
    }

    #[test]
    fn wake_and_ready_fd_are_reported_together() {
        let mut looper = Looper::new().expect("eventfd + epoll");
        let mut pipe = TestPipe::new();
        looper
            .add_fd(pipe.read_fd(), ALOOPER_EVENT_INPUT, FdHandler::Ident(3), 0)
            .expect("add_fd");
        pipe.signal();
        looper.waker().wake();
        assert_eq!(
            looper.poller().poll(100),
            PollOutcome::Ready(vec![ReadyFd {
                fd: pipe.read_fd(),
                events: ALOOPER_EVENT_INPUT
            }])
        );
        assert_eq!(ident_responses(&looper, 100)[0].0, 3);
        assert!(looper.remove_fd(pipe.read_fd()));
        assert_eq!(
            looper.poller().poll(0),
            PollOutcome::Timeout,
            "the wake was consumed by the poll that reported the fd"
        );
    }

    #[test]
    fn remove_fd_stops_it_from_firing() {
        let mut looper = Looper::new().expect("eventfd + epoll");
        let mut pipe = TestPipe::new();
        looper
            .add_fd(pipe.read_fd(), ALOOPER_EVENT_INPUT, FdHandler::Ident(9), 0)
            .expect("add_fd");
        assert!(looper.remove_fd(pipe.read_fd()), "fd was present");
        assert!(!looper.remove_fd(pipe.read_fd()), "now absent");
        pipe.signal();

        assert_eq!(looper.poller().poll(10), PollOutcome::Timeout);
    }

    #[test]
    fn a_replaced_registration_survives_removal_of_the_old_one() {
        let mut looper = Looper::new().expect("eventfd + epoll");
        let mut pipe = TestPipe::new();
        looper
            .add_fd(pipe.read_fd(), ALOOPER_EVENT_INPUT, FdHandler::Ident(1), 0)
            .expect("add_fd");
        pipe.signal();
        let old = match looper.poller().poll(100) {
            PollOutcome::Ready(ready) => looper.responses(&ready)[0].registration,
            other => panic!("expected ready, got {other:?}"),
        };
        looper
            .add_fd(pipe.read_fd(), ALOOPER_EVENT_INPUT, FdHandler::Ident(2), 0)
            .expect("re-add");
        assert!(!looper.remove_registration(&old));
        assert_eq!(ident_responses(&looper, 100)[0].0, 2);
    }

    fn poll_events(looper: &Looper) -> i32 {
        ident_responses(looper, 100)[0].2
    }

    #[test]
    fn output_registration_reports_writability() {
        let mut looper = Looper::new().expect("eventfd + epoll");
        let pipe = TestPipe::new();
        looper
            .add_fd(
                pipe.write.as_raw_fd(),
                ALOOPER_EVENT_OUTPUT,
                FdHandler::Ident(4),
                0,
            )
            .expect("add_fd");
        assert_eq!(poll_events(&looper), ALOOPER_EVENT_OUTPUT);
    }

    #[test]
    fn writer_close_reports_hangup() {
        let mut looper = Looper::new().expect("eventfd + epoll");
        let TestPipe { read, write } = TestPipe::new();
        looper
            .add_fd(
                read.as_raw_fd(),
                ALOOPER_EVENT_INPUT,
                FdHandler::Ident(5),
                0,
            )
            .expect("add_fd");
        drop(write);
        let events = poll_events(&looper);
        assert!(events & ALOOPER_EVENT_HANGUP != 0, "events {events:#x}");
        assert_eq!(events & ALOOPER_EVENT_INVALID, 0, "events {events:#x}");
    }

    #[test]
    fn reader_close_reports_error() {
        let mut looper = Looper::new().expect("eventfd + epoll");
        let TestPipe { read, write } = TestPipe::new();
        looper
            .add_fd(
                write.as_raw_fd(),
                ALOOPER_EVENT_OUTPUT,
                FdHandler::Ident(6),
                0,
            )
            .expect("add_fd");
        drop(read);
        let events = poll_events(&looper);
        assert!(events & ALOOPER_EVENT_ERROR != 0, "events {events:#x}");
    }

    #[test]
    fn add_fd_twice_replaces_not_duplicates() {
        let mut looper = Looper::new().expect("eventfd + epoll");
        let pipe = TestPipe::new();
        looper
            .add_fd(pipe.read_fd(), ALOOPER_EVENT_INPUT, FdHandler::Ident(1), 0)
            .expect("add_fd");
        looper
            .add_fd(pipe.read_fd(), ALOOPER_EVENT_INPUT, FdHandler::Ident(2), 0)
            .expect("re-add_fd");
        assert_eq!(
            looper.registrations.len(),
            1,
            "re-add replaces the registration"
        );
        assert!(
            matches!(looper.registrations[0].handler, FdHandler::Ident(2)),
            "ident updated to the latest"
        );
    }

    #[test]
    fn add_fd_of_an_invalid_fd_is_an_error() {
        let mut looper = Looper::new().expect("eventfd + epoll");
        assert!(looper
            .add_fd(-1, ALOOPER_EVENT_INPUT, FdHandler::Ident(1), 0)
            .is_err());
        assert!(looper.registrations.is_empty());
    }
}
