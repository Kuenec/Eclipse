//! SCM_RIGHTS fd passing over the control `UnixStream` — how the memfd of a
//! [`super::proto::HelperMsg::FrameBufferNew`] generation crosses the process boundary.
//!
//! 2026-07-03: `SOCK_STREAM` ancillary data is POSITIONAL — the fd rides on exactly one byte
//! of the stream (the sentinel the helper sends immediately after the `FrameBufferNew` frame
//! bytes). The protocol reader uses exact-length reads and never buffers ahead
//! ([`super::proto`] module docs), so when the consumer has decoded `FrameBufferNew` the next
//! unread byte is exactly the sentinel and ONE `recvmsg` takes ownership of the fd. A
//! buffered reader on the control socket would consume the sentinel with a plain `read` and
//! silently discard the fd — pinned by the unit test below.
//!
//! The `/proc/<pid>/fd/<n>` same-uid alternative was REJECTED (2026-07-03): opening another
//! process's fd link is subject to the ptrace access-mode check and Yama `ptrace_scope`
//! varies across distros (scope>=2 breaks even parent→child) — `SCM_RIGHTS` is the
//! kernel-portable mechanism (detect-don't-assume, AGENTS.md §2.9).
//!
//! `unsafe` is confined to the `sendmsg`/`recvmsg` FFI (the `loader/looper.rs` /
//! `loader/opensl.rs` fd-owning house shape); this file is shared into the helper crate by
//! `#[path]`, so it uses `libc` only (the helper does not depend on `rustix`).

use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

/// The single stream byte the memfd's `SCM_RIGHTS` control message rides on.
pub const FD_SENTINEL: u8 = 0xF5;

/// Typed fd-passing error (payload-free, like [`super::proto::ProtoError`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FdPassError {
    /// `sendmsg`/`recvmsg` failed at the OS level.
    Io(std::io::ErrorKind),
    /// The peer closed the stream where the sentinel byte was expected.
    Disconnected,
    /// The byte at the fd-passing position was not [`FD_SENTINEL`] — stream framing is
    /// desynchronized (e.g. a buffered reader consumed the sentinel).
    BadSentinel { value: u8 },
    /// The sentinel byte arrived without an attached fd (or the peer sent none).
    NoFdReceived,
    /// The kernel truncated the control message (`MSG_CTRUNC`) — fd delivery is unreliable.
    ControlTruncated,
    /// The control message carried a number of fds other than exactly one.
    WrongFdCount { count: usize },
}

impl std::fmt::Display for FdPassError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(kind) => write!(f, "fd-pass I/O error: {kind}"),
            Self::Disconnected => write!(f, "peer closed the stream at the fd sentinel"),
            Self::BadSentinel { value } => write!(
                f,
                "expected fd sentinel 0x{FD_SENTINEL:02X}, got 0x{value:02X} (stream desync)"
            ),
            Self::NoFdReceived => write!(f, "sentinel byte carried no SCM_RIGHTS fd"),
            Self::ControlTruncated => write!(f, "SCM_RIGHTS control message truncated"),
            Self::WrongFdCount { count } => {
                write!(f, "SCM_RIGHTS carried {count} fds (expected exactly 1)")
            }
        }
    }
}

impl std::error::Error for FdPassError {}

/// Control-message buffer sized/aligned for exactly one fd. 64 bytes covers
/// `CMSG_SPACE(sizeof(int))` (24 on x86-64) with room to observe over-delivery.
#[repr(align(8))]
struct CmsgBuf([u8; 64]);

/// Helper role: send [`FD_SENTINEL`] with `fd` attached as `SCM_RIGHTS` ancillary data. Call
/// immediately after writing the `FrameBufferNew` frame bytes to the same stream.
pub fn send_fd_with_sentinel(stream: &UnixStream, fd: BorrowedFd<'_>) -> Result<(), FdPassError> {
    let mut sentinel = FD_SENTINEL;
    let mut iov = libc::iovec {
        iov_base: (&mut sentinel as *mut u8).cast(),
        iov_len: 1,
    };
    let mut cbuf = CmsgBuf([0; 64]);
    // SAFETY: msghdr is a plain-old-data struct; zeroing then setting the fields we use is
    // the documented way to build one (2026-07-03).
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.0.as_mut_ptr().cast();
    // SAFETY: CMSG_SPACE/CMSG_LEN are pure size computations for one int-sized payload.
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) } as usize;
    debug_assert!(msg.msg_controllen <= cbuf.0.len());
    // SAFETY: msg_control points at cbuf (64 bytes, 8-aligned) and msg_controllen fits in it,
    // so CMSG_FIRSTHDR returns a valid, writable header inside cbuf; CMSG_DATA points at its
    // int-sized payload area. We write exactly one raw fd there.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) as usize;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<i32>(), fd.as_raw_fd());
    }
    loop {
        // SAFETY: `msg` and everything it points to (iov, cbuf) outlive this call; the fd is
        // a live BorrowedFd. MSG_NOSIGNAL keeps a dead peer an error, not a SIGPIPE.
        let n = unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) };
        if n == 1 {
            return Ok(());
        }
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(FdPassError::Io(err.kind()));
        }
        // 0 bytes sent for a 1-byte message: treat as a dead peer.
        return Err(FdPassError::Disconnected);
    }
}

/// Consumer role: receive the sentinel byte + its `SCM_RIGHTS` fd. Call exactly once,
/// immediately after decoding a `FrameBufferNew` frame with exact-length reads.
pub fn recv_fd_after_sentinel(stream: &UnixStream) -> Result<OwnedFd, FdPassError> {
    let mut sentinel: u8 = 0;
    let mut iov = libc::iovec {
        iov_base: (&mut sentinel as *mut u8).cast(),
        iov_len: 1,
    };
    let mut cbuf = CmsgBuf([0; 64]);
    // SAFETY: as in send — POD msghdr, fields point at live local buffers.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.0.as_mut_ptr().cast();
    msg.msg_controllen = cbuf.0.len();
    let n = loop {
        // SAFETY: `msg` and its buffers outlive the call. MSG_CMSG_CLOEXEC atomically sets
        // CLOEXEC on every received fd (no leak window into later fork/exec).
        let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
        if n >= 0 {
            break n;
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(FdPassError::Io(err.kind()));
        }
    };
    if n == 0 {
        return Err(FdPassError::Disconnected);
    }

    // Collect every delivered fd FIRST so an error path can close them all (no fd leaks).
    let mut fds: Vec<OwnedFd> = Vec::new();
    // SAFETY: recvmsg returned >= 0, so msg/cbuf describe kernel-written control data;
    // CMSG_FIRSTHDR/CMSG_NXTHDR walk only within msg_controllen. Each SCM_RIGHTS payload int
    // is an fd the kernel just installed in this process — wrapping it in OwnedFd takes the
    // ownership the kernel handed us (exactly once per fd).
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let payload = (*cmsg).cmsg_len - libc::CMSG_LEN(0) as usize;
                let count = payload / std::mem::size_of::<i32>();
                let data = libc::CMSG_DATA(cmsg).cast::<i32>();
                for i in 0..count {
                    let raw = std::ptr::read_unaligned(data.add(i));
                    fds.push(OwnedFd::from_raw_fd(raw));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }

    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        // Dropping `fds` closes anything partially delivered.
        return Err(FdPassError::ControlTruncated);
    }
    if sentinel != FD_SENTINEL {
        return Err(FdPassError::BadSentinel { value: sentinel });
    }
    match fds.len() {
        0 => Err(FdPassError::NoFdReceived),
        1 => Ok(fds.remove(0)),
        count => Err(FdPassError::WrongFdCount { count }),
    }
}

#[cfg(test)]
mod tests {
    use super::super::proto::{read_helper_msg, HelperMsg};
    use super::super::shm;
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::os::fd::AsFd;
    use std::os::unix::fs::{FileExt, MetadataExt};

    #[test]
    fn fdpass_sends_and_receives_memfd_across_socketpair() {
        // 2026-07-03: pins the cmsg/stream-boundary discipline — the FrameBufferNew frame
        // bytes are decoded with exact-length reads, so the very next byte is the sentinel
        // and one recvmsg yields the fd. A refactor that introduces a buffered reader on the
        // control socket (swallowing the sentinel and dropping the fd) fails here.
        let (helper_end, consumer_end) = UnixStream::pair().expect("socketpair");

        // Helper role: a real sealed frame memfd with known bytes written into slot 0.
        let (memfd, slot_bytes) =
            shm::create_sealed_frame_memfd(4, 2, 2).expect("create sealed memfd");
        assert_eq!(slot_bytes, 4 * 2 * 4);
        let payload: Vec<u8> = (0..slot_bytes as usize)
            .map(|i| (i * 7 + 3) as u8)
            .collect();
        let memfile = File::from(memfd.try_clone().expect("dup memfd"));
        memfile.write_at(&payload, 0).expect("write payload");

        let announce = HelperMsg::FrameBufferNew {
            view: 42,
            generation: 1,
            width: 4,
            height: 2,
            stride: 16,
            slot_bytes,
            slot_count: 2,
        };
        let frame_bytes = announce.encode().expect("encode");
        (&helper_end)
            .write_all(&frame_bytes)
            .expect("write announce frame");
        send_fd_with_sentinel(&helper_end, memfd.as_fd()).expect("send fd");

        // Consumer role: exact-length frame decode, then exactly one recvmsg for the fd.
        let mut reader = &consumer_end;
        let decoded = read_helper_msg(&mut reader).expect("decode announce");
        assert_eq!(decoded, announce);
        let received = recv_fd_after_sentinel(&consumer_end).expect("recv fd");

        // fstat identity: the received fd names the same inode as the sent memfd.
        let sent_meta = memfile.metadata().expect("sent metadata");
        let recv_file = File::from(received);
        let recv_meta = recv_file.metadata().expect("recv metadata");
        assert_eq!(sent_meta.ino(), recv_meta.ino());
        assert_eq!(sent_meta.dev(), recv_meta.dev());

        // Read back the helper-written bytes through the received fd.
        let mut back = vec![0u8; payload.len()];
        recv_file.read_exact_at(&mut back, 0).expect("read back");
        assert_eq!(back, payload);

        // The stream is now empty — nothing after the sentinel was consumed or left over.
        consumer_end.set_nonblocking(true).expect("set nonblocking");
        let mut probe = [0u8; 1];
        let err = std::io::Read::read(&mut (&consumer_end), &mut probe)
            .expect_err("no trailing bytes after the sentinel");
        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
    }
}
