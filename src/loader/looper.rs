//! Real fd-backed, wakeable `ALooper` core — the standard NDK looper event model.
//!
//! 2026-06-05: libroblox's native engine imports **7 `ALooper_*` natives** (`prepare`/`forThread`/
//! `acquire`/`release`/`pollOnce`/`addFd`/`removeFd`) and uses them the standard NDK way: a worker
//! thread `ALooper_prepare`s a looper, `ALooper_addFd`s the file descriptors it cares about (each with
//! an `ident` and the events it wants), then loops on `ALooper_pollOnce(timeout, …)`, which **blocks
//! until one of those fds is ready** (or the timeout expires / the looper is woken) and reports which
//! fd fired. The previous Eclipse looper tracked the registered fds for bookkeeping but never actually
//! polled them — `pollOnce` could only return the no-source `ALOOPER_POLL_TIMEOUT`/`_ERROR` sentinel.
//!
//! This module makes the looper **real**: each looper owns a wake [`eventfd(2)`] plus its registered
//! `(fd, ident, events)` poll set, and [`PollSnapshot::poll_once`] does a genuine [`poll(2)`] over the
//! wake fd + every registered fd with the caller's timeout, returning the standard NDK outcome:
//!
//! - a registered fd became ready (and was added with no callback) → its `ident` (≥ 0) + the fd +
//!   the events, exactly the NDK `ALooper_pollOnce` "return the ident" contract;
//! - the looper was woken via [`Waker::wake`] (or a winit input event woke it) → `ALOOPER_POLL_WAKE`;
//! - the timeout expired with nothing ready → `ALOOPER_POLL_TIMEOUT`;
//! - the `poll(2)` syscall itself failed → `ALOOPER_POLL_ERROR`.
//!
//! ## Lock discipline (why `poll_once` is on a snapshot, not on `Looper`)
//! A [`Looper`] lives in the [`crate::loader::ndk_registry`] generational slab behind an `ALooper*`,
//! guarded by the slab's mutex. `ALooper_pollOnce` can **block indefinitely**; if it blocked while
//! holding the slab mutex, a concurrent [`Waker::wake`] (or `addFd` from another thread) would
//! deadlock. So the native takes a cheap [`PollSnapshot`] under the lock — a clone of the shared wake
//! `eventfd` (`Arc<OwnedFd>`) plus a copy of the small poll set — **releases the lock**, and does the
//! blocking `poll(2)` lock-free. The wake fd is an [`Arc`]-shared kernel object, so a [`Waker`] (also
//! obtained lock-free) writes to the *same* eventfd a parked snapshot is polling. The eventfd is
//! drained on the snapshot after a wake, resetting the same shared kernel counter.
//!
//! `#![forbid(unsafe_code)]` is **not** set here: the looper owns OS file descriptors and issues the
//! `eventfd`/`poll`/`read`/`write` syscalls. Every such block carries a dated `// SAFETY:` note
//! (AGENTS.md §2.3). The owned wake fd is RAII-closed when the last `Arc` drops. No `%fs`/TLS, no
//! foreign-code execution, no native-load/linker code — a self-contained event primitive.
//!
//! Soundness: a stale/fabricated `ALooper*` is rejected by the registry before reaching this code
//! (never a wild fd). The fds the engine registers via `ALooper_addFd` are the engine's own (e.g. its
//! own pipe/eventfd wake sources) — Eclipse polls them, it does not own or close them.

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;

/// `ALOOPER_POLL_WAKE` = -1: the poll was woken before any fd fired (e.g. [`Waker::wake`]). From
/// `<android/looper.h>`.
pub const ALOOPER_POLL_WAKE: i32 = -1;
/// `ALOOPER_POLL_CALLBACK` = -2: a registered fd with a callback fired (Eclipse's looper does not run
/// callbacks — see the `ALooper_addFd` native — so it never returns this; defined for completeness).
/// From `<android/looper.h>`.
pub const ALOOPER_POLL_CALLBACK: i32 = -2;
/// `ALOOPER_POLL_TIMEOUT` = -3: the timeout expired before any fd became ready. From `<android/looper.h>`.
pub const ALOOPER_POLL_TIMEOUT: i32 = -3;
/// `ALOOPER_POLL_ERROR` = -4: the underlying `poll(2)` failed. From `<android/looper.h>`.
pub const ALOOPER_POLL_ERROR: i32 = -4;

/// `ALOOPER_EVENT_INPUT` = 1 (POLLIN): the fd has data to read. From `<android/looper.h>`; equals
/// `POLLIN` by construction in the NDK.
pub const ALOOPER_EVENT_INPUT: i32 = 1;

/// One registered file descriptor in a looper's poll set: the fd, the caller's `ident` (returned by
/// `pollOnce` when this fd fires with no callback), and the requested events mask (NDK
/// `ALOOPER_EVENT_*`, which map 1:1 onto `poll(2)` `POLL*` flags).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PollFd {
    fd: i32,
    ident: i32,
    events: i32,
}

/// The outcome of a single [`PollSnapshot::poll_once`] — modelled so the `ALooper_pollOnce` native can
/// map it to the exact `int` return + out-params the NDK contract specifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollResult {
    /// A registered fd (added with no callback) became ready: `(ident, fd, events)`. The native
    /// returns `ident`, and writes `fd`/`events` to the out-params.
    Fd { ident: i32, fd: i32, events: i32 },
    /// The looper was woken via [`Waker::wake`] before any fd fired. The native returns
    /// [`ALOOPER_POLL_WAKE`].
    Wake,
    /// The timeout expired with nothing ready. The native returns [`ALOOPER_POLL_TIMEOUT`].
    Timeout,
    /// The underlying `poll(2)` failed. The native returns [`ALOOPER_POLL_ERROR`].
    Error,
}

/// A real, wakeable NDK looper: a shared wake `eventfd` plus the registered `(fd, ident, events)` poll
/// set. Lives in the [`crate::loader::ndk_registry`] slab behind an `ALooper*`. The blocking poll runs
/// on a [`PollSnapshot`] taken under the slab lock then released — see the module-level lock discipline.
#[derive(Debug)]
pub struct Looper {
    /// The internal wake source, shared (`Arc`) so a [`Waker`] and a [`PollSnapshot`] reference the
    /// same kernel eventfd without the slab lock. A counter `eventfd` (`EFD_CLOEXEC | EFD_NONBLOCK`):
    /// [`Waker::wake`] writes 1; a snapshot's poll includes it and drains it on fire → [`PollResult::Wake`].
    wake_fd: Arc<OwnedFd>,
    /// The registered fds from `ALooper_addFd`, removed by `ALooper_removeFd`. Eclipse does not own
    /// these (they are the engine's own sources) — it only polls them.
    fds: Vec<PollFd>,
}

impl Looper {
    /// Create a looper with a fresh wake `eventfd`. Returns `None` if `eventfd(2)` fails (fd
    /// exhaustion) — the `ALooper_prepare` native then returns NULL, the NDK "no looper" answer.
    pub fn new() -> Option<Self> {
        // SAFETY: 2026-06-05 — `eventfd(2)` with a 0 initial count and CLOEXEC|NONBLOCK flags; the
        // returned fd is taken ownership of immediately below (RAII close), or the call failed (-1).
        let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if raw < 0 {
            return None;
        }
        // SAFETY: 2026-06-05 — `raw` is a fresh, valid, exclusively-owned fd from the `eventfd` call
        // above; wrapping it in `OwnedFd` transfers ownership so it is closed exactly once on Drop.
        let wake_fd = unsafe { owned_from_raw(raw) };
        Some(Self {
            wake_fd: Arc::new(wake_fd),
            fds: Vec::new(),
        })
    }

    /// Register `fd` with this looper under `ident` for `events` (NDK `ALooper_addFd`). A second
    /// `addFd` for the same fd **replaces** its registration (the NDK contract), so re-adding updates
    /// `ident`/`events` rather than duplicating. The NDK requires `ident >= 0` when no callback is
    /// used; a negative `ident` is rejected by the native before reaching here.
    pub fn add_fd(&mut self, fd: i32, ident: i32, events: i32) {
        if let Some(existing) = self.fds.iter_mut().find(|p| p.fd == fd) {
            existing.ident = ident;
            existing.events = events;
        } else {
            self.fds.push(PollFd { fd, ident, events });
        }
    }

    /// Unregister all entries for `fd` (NDK `ALooper_removeFd`). Returns whether an entry was present
    /// (the native returns 1 regardless, per the "removed or not present" contract).
    pub fn remove_fd(&mut self, fd: i32) -> bool {
        let before = self.fds.len();
        self.fds.retain(|p| p.fd != fd);
        self.fds.len() != before
    }

    /// A cheap [`Waker`] for this looper that can wake a parked `poll_once` **without** the slab lock
    /// (it clones the shared wake `eventfd` `Arc`). The winit input feed holds one of these.
    pub fn waker(&self) -> Waker {
        Waker {
            wake_fd: Arc::clone(&self.wake_fd),
        }
    }

    /// Take a [`PollSnapshot`] for a blocking poll: a clone of the shared wake fd + a copy of the
    /// current poll set. Cheap (an `Arc` bump + a small `Vec` copy). The caller takes this under the
    /// slab lock, **drops the lock**, then blocks on [`PollSnapshot::poll_once`] — see the module lock
    /// discipline (never block while holding the slab mutex).
    pub fn snapshot(&self) -> PollSnapshot {
        PollSnapshot {
            wake_fd: Arc::clone(&self.wake_fd),
            fds: self.fds.clone(),
        }
    }
}

/// A lock-free handle that wakes a parked `poll_once` by writing the looper's shared wake `eventfd`.
/// Obtained via [`Looper::waker`]; safe to hold and call from any thread (e.g. the winit event loop).
#[derive(Debug, Clone)]
pub struct Waker {
    wake_fd: Arc<OwnedFd>,
}

impl Waker {
    /// Wake a (possibly parked) `poll_once`. This is what a winit input event (engine path) calls to
    /// unblock the engine's looper thread so it re-checks its input source. Errors are swallowed: a
    /// full eventfd counter already means "wake pending", and a wake is best-effort by design (the NDK
    /// `ALooper_wake` is `void`).
    pub fn wake(&self) {
        write_wake(self.wake_fd.as_raw_fd());
    }
}

/// A point-in-time copy of a looper's poll set + a clone of its shared wake fd, on which the blocking
/// `poll(2)` runs **without** the slab lock held (see the module lock discipline).
#[derive(Debug)]
pub struct PollSnapshot {
    wake_fd: Arc<OwnedFd>,
    fds: Vec<PollFd>,
}

impl PollSnapshot {
    /// Wait up to `timeout_millis` for the wake fd or any registered fd to become ready, the real
    /// `ALooper_pollOnce` semantics. `timeout_millis < 0` blocks indefinitely; `0` returns immediately;
    /// `> 0` waits that long. The wake fd is checked first, so a pending wake is reported as
    /// [`PollResult::Wake`] even alongside ready fds (the NDK drains the wake before reporting fds).
    pub fn poll_once(&self, timeout_millis: i32) -> PollResult {
        // Build the poll set: the wake fd first, then every registered fd. The engine registers only a
        // handful of fds and `pollOnce` blocks (not a per-frame allocation site), so a plain `Vec` is
        // fine and clearer than a fixed small-vec.
        let mut pfds: Vec<libc::pollfd> = Vec::with_capacity(self.fds.len() + 1);
        pfds.push(libc::pollfd {
            fd: self.wake_fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        for p in &self.fds {
            pfds.push(libc::pollfd {
                fd: p.fd,
                // The NDK ALOOPER_EVENT_* flags equal the poll(2) POLL* flags by construction; mask to
                // the short the kernel expects.
                events: (p.events & i32::from(u16::MAX)) as libc::c_short,
                revents: 0,
            });
        }

        // SAFETY: 2026-06-05 — `pfds` is a valid, initialized, mutable slice of `len` `pollfd`s; the
        // kernel writes only their `revents`. `len` fits `nfds_t` (a handful of fds). `timeout` is the
        // caller's millis (negative = block forever, the poll(2) contract).
        let rc = unsafe {
            libc::poll(
                pfds.as_mut_ptr(),
                pfds.len() as libc::nfds_t,
                timeout_millis,
            )
        };

        if rc < 0 {
            // poll(2) failed. EINTR is a spurious wake, not an error condition; report WAKE so the
            // caller re-polls (the NDK contract: any return may imply WAKE). Anything else → ERROR.
            return if last_errno() == libc::EINTR {
                PollResult::Wake
            } else {
                PollResult::Error
            };
        }
        if rc == 0 {
            return PollResult::Timeout;
        }

        // The wake fd is index 0. If it fired, drain it and report WAKE (NDK: the wake is consumed and
        // reported before fd events).
        if pfds[0].revents & libc::POLLIN != 0 {
            drain_eventfd(self.wake_fd.as_raw_fd());
            return PollResult::Wake;
        }

        // Otherwise report the first registered fd that became ready, with its ident.
        for (slot, p) in pfds[1..].iter().zip(self.fds.iter()) {
            if slot.revents != 0 {
                return PollResult::Fd {
                    ident: p.ident,
                    fd: p.fd,
                    events: i32::from(slot.revents),
                };
            }
        }

        // rc > 0 but neither the wake fd nor a tracked fd reported (a race with concurrent removal,
        // say). Be conservative: treat as a wake so the caller re-polls rather than spins.
        PollResult::Wake
    }
}

/// Write one wake to a counter `eventfd` (best-effort; an already-saturated counter still means
/// "wake pending", so a failure is ignored).
fn write_wake(fd: i32) {
    let one: u64 = 1;
    // SAFETY: 2026-06-05 — write exactly 8 bytes (one `u64`, the eventfd write size) from a valid
    // readable 8-byte source to the wake eventfd. A failure (e.g. EAGAIN on a saturated counter) is
    // intentionally ignored — a non-zero counter already means a wake is pending.
    let _ = unsafe {
        libc::write(
            fd,
            std::ptr::addr_of!(one).cast(),
            std::mem::size_of::<u64>(),
        )
    };
}

/// Drain a counter `eventfd` back to 0 so a single wake produces a single [`PollResult::Wake`] (the
/// eventfd read returns the accumulated count and resets it).
fn drain_eventfd(fd: i32) {
    let mut buf: u64 = 0;
    // SAFETY: 2026-06-05 — read up to 8 bytes from the non-blocking wake eventfd into a valid 8-byte
    // buffer. The read returns the counter and resets it to 0; EAGAIN (nothing to read) is ignored.
    let _ = unsafe {
        libc::read(
            fd,
            std::ptr::addr_of_mut!(buf).cast(),
            std::mem::size_of::<u64>(),
        )
    };
}

/// Wrap a raw fd in an `OwnedFd` (RAII close). Confined helper so the one `from_raw_fd` `unsafe` has a
/// single dated site.
///
/// # Safety
/// `raw` must be a valid, open file descriptor that the caller exclusively owns and is transferring;
/// the returned `OwnedFd` closes it exactly once on Drop.
unsafe fn owned_from_raw(raw: i32) -> OwnedFd {
    use std::os::fd::FromRawFd;
    // SAFETY: 2026-06-05 — delegated to the caller's contract above; `raw` is the fresh exclusively
    // owned fd just returned by `eventfd(2)`.
    unsafe { OwnedFd::from_raw_fd(raw) }
}

/// The current thread's `errno`, read after a failing libc call.
fn last_errno() -> i32 {
    // SAFETY: 2026-06-05 — `__errno_location` returns a valid pointer to the calling thread's errno;
    // reading it immediately after a failed libc syscall is the standard, sound errno access.
    unsafe { *libc::__errno_location() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::time::Instant;

    /// A self-contained pipe pair for tests: writing the write end makes the read end POLLIN-ready,
    /// simulating an engine input source the looper polls. RAII-closes both ends.
    struct TestPipe {
        read: OwnedFd,
        write: std::fs::File,
    }

    impl TestPipe {
        fn new() -> Self {
            let mut fds = [0i32; 2];
            // SAFETY: test-only — pipe2 writes two fresh fds into the 2-element array; both are taken
            // into RAII owners below.
            let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
            assert_eq!(rc, 0, "pipe2 failed");
            // SAFETY: test-only — fds[0]/fds[1] are fresh exclusively-owned fds from pipe2.
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
        // No registered fds, no wake → a finite timeout must return Timeout (not block, not fake).
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
        // Pre-wake then poll: the wake fd is already readable, so even a 0ms poll reports Wake.
        looper.waker().wake();
        assert_eq!(looper.snapshot().poll_once(0), PollResult::Wake);
        // The wake was drained → a subsequent poll times out (one wake → one Wake).
        assert_eq!(looper.snapshot().poll_once(0), PollResult::Timeout);
    }

    #[test]
    fn wake_from_another_thread_unblocks_a_parked_poll() {
        let looper = Looper::new().expect("eventfd");
        let waker = looper.waker(); // lock-free handle, moved to the other thread
                                    // Park on an indefinite poll; another thread wakes it. Proves the fd genuinely unblocks poll.
        let h = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            waker.wake();
        });
        let snap = looper.snapshot();
        let start = Instant::now();
        // Negative timeout = block forever; only the wake can return it.
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
        // Nothing written yet → timeout.
        assert_eq!(looper.snapshot().poll_once(10), PollResult::Timeout);
        // Signal the source → poll returns the fd's ident.
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
        pipe.signal(); // fd is ready ...
        looper.waker().wake(); // ... and a wake is pending: the wake is reported first.
        assert_eq!(looper.snapshot().poll_once(100), PollResult::Wake);
        // After draining the wake, the still-ready fd is reported.
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
        // With the fd removed, the ready source is not in the poll set → timeout.
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
