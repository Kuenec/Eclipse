use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

pub const FD_SENTINEL: u8 = 0xF5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FdPassError {
    Io(std::io::ErrorKind),

    Disconnected,

    BadSentinel { value: u8 },

    NoFdReceived,

    ControlTruncated,

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

#[repr(align(8))]
struct CmsgBuf([u8; 64]);

pub fn send_fd_with_sentinel(stream: &UnixStream, fd: BorrowedFd<'_>) -> Result<(), FdPassError> {
    let mut sentinel = FD_SENTINEL;
    let mut iov = libc::iovec {
        iov_base: (&mut sentinel as *mut u8).cast(),
        iov_len: 1,
    };
    let mut cbuf = CmsgBuf([0; 64]);

    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.0.as_mut_ptr().cast();

    msg.msg_controllen = unsafe { libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) } as usize;
    debug_assert!(msg.msg_controllen <= cbuf.0.len());

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) as usize;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<i32>(), fd.as_raw_fd());
    }
    loop {
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

        return Err(FdPassError::Disconnected);
    }
}

pub fn recv_fd_after_sentinel(stream: &UnixStream) -> Result<OwnedFd, FdPassError> {
    let mut sentinel: u8 = 0;
    let mut iov = libc::iovec {
        iov_base: (&mut sentinel as *mut u8).cast(),
        iov_len: 1,
    };
    let mut cbuf = CmsgBuf([0; 64]);

    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.0.as_mut_ptr().cast();
    msg.msg_controllen = cbuf.0.len();
    let n = loop {
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

    let mut fds: Vec<OwnedFd> = Vec::new();

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
        let (helper_end, consumer_end) = UnixStream::pair().expect("socketpair");

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

        let mut reader = &consumer_end;
        let decoded = read_helper_msg(&mut reader).expect("decode announce");
        assert_eq!(decoded, announce);
        let received = recv_fd_after_sentinel(&consumer_end).expect("recv fd");

        let sent_meta = memfile.metadata().expect("sent metadata");
        let recv_file = File::from(received);
        let recv_meta = recv_file.metadata().expect("recv metadata");
        assert_eq!(sent_meta.ino(), recv_meta.ino());
        assert_eq!(sent_meta.dev(), recv_meta.dev());

        let mut back = vec![0u8; payload.len()];
        recv_file.read_exact_at(&mut back, 0).expect("read back");
        assert_eq!(back, payload);

        consumer_end.set_nonblocking(true).expect("set nonblocking");
        let mut probe = [0u8; 1];
        let err = std::io::Read::read(&mut (&consumer_end), &mut probe)
            .expect_err("no trailing bytes after the sentinel");
        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
    }
}
