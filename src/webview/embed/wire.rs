use std::collections::VecDeque;
use std::ffi::CString;
use std::fmt;
use std::io::{self, IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;

use rustix::net::{
    recvmsg, sendmsg, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags,
    SendAncillaryBuffer, SendAncillaryMessage, SendFlags,
};
use wayland_client::backend::protocol::{AllowNull, Argument, ArgumentType};

pub(super) const MAX_FDS_PER_SEND: usize = 28;

pub(super) const MAX_REQUEST_SIZE: usize = 4096;

const HEADER_SIZE: usize = 8;

const MAX_MESSAGE_SIZE: usize = 0xfffc;

const READ_CHUNK: usize = 16 * 1024;

const RX_BYTE_LIMIT: usize = 128 * 1024;

const RX_FD_LIMIT: usize = 128;

const TX_BYTE_LIMIT: usize = 4 * 1024 * 1024;

const TX_FD_LIMIT: usize = 256;

pub(super) type Args = Vec<Argument<u32, OwnedFd>>;

#[derive(Debug)]
pub(super) struct Message {
    pub(super) object: u32,
    pub(super) opcode: u16,
    pub(super) args: Args,
}

#[derive(Debug)]
pub(super) enum WireError {
    Io(io::Error),
    Closed,
    Malformed {
        object: u32,
        opcode: u16,
        what: &'static str,
    },
    Backlog(&'static str),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "socket error: {error}"),
            Self::Closed => f.write_str("the peer closed its socket"),
            Self::Malformed {
                object,
                opcode,
                what,
            } => write!(f, "malformed message {opcode} on object {object}: {what}"),
            Self::Backlog(what) => write!(f, "{what}"),
        }
    }
}

impl From<rustix::io::Errno> for WireError {
    fn from(errno: rustix::io::Errno) -> Self {
        Self::Io(errno.into())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Header {
    pub(super) object: u32,
    pub(super) opcode: u16,
    size: usize,
}

pub(super) struct Wire {
    stream: UnixStream,
    max_message: usize,
    rx: Vec<u8>,
    rx_fds: VecDeque<OwnedFd>,
    tx: Vec<u8>,
    tx_fds: VecDeque<OwnedFd>,
    tx_fd_ends: VecDeque<(usize, usize)>,
}

impl Wire {
    pub(super) fn requests(stream: UnixStream) -> io::Result<Self> {
        Self::new(stream, MAX_REQUEST_SIZE)
    }

    #[cfg(test)]
    pub(super) fn events(stream: UnixStream) -> io::Result<Self> {
        Self::new(stream, MAX_MESSAGE_SIZE)
    }

    fn new(stream: UnixStream, max_message: usize) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            max_message,
            rx: Vec::with_capacity(READ_CHUNK),
            rx_fds: VecDeque::new(),
            tx: Vec::with_capacity(READ_CHUNK),
            tx_fds: VecDeque::new(),
            tx_fd_ends: VecDeque::new(),
        })
    }

    pub(super) fn fd(&self) -> BorrowedFd<'_> {
        self.stream.as_fd()
    }

    pub(super) fn wants_write(&self) -> bool {
        !self.tx.is_empty()
    }

    pub(super) fn receive(&mut self) -> Result<(), WireError> {
        let mut chunk = [0u8; READ_CHUNK];
        while self.rx.len() + READ_CHUNK <= RX_BYTE_LIMIT {
            let mut space =
                [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS_PER_SEND))];
            let mut control = RecvAncillaryBuffer::new(&mut space);
            let mut iov = [IoSliceMut::new(&mut chunk)];
            let received = match recvmsg(
                &self.stream,
                &mut iov,
                &mut control,
                RecvFlags::CMSG_CLOEXEC | RecvFlags::DONTWAIT,
            ) {
                Ok(received) => received,
                Err(rustix::io::Errno::AGAIN) => return Ok(()),
                Err(rustix::io::Errno::INTR) => continue,
                Err(errno) => return Err(errno.into()),
            };
            for message in control.drain() {
                if let RecvAncillaryMessage::ScmRights(fds) = message {
                    self.rx_fds.extend(fds);
                }
            }
            if received.flags.contains(ReturnFlags::CTRUNC) {
                return Err(WireError::Backlog(
                    "more file descriptors than one message carries",
                ));
            }
            if self.rx_fds.len() > RX_FD_LIMIT {
                return Err(WireError::Backlog("too many unread file descriptors"));
            }
            if received.bytes == 0 {
                return Err(WireError::Closed);
            }
            self.rx.extend_from_slice(&chunk[..received.bytes]);
        }
        Ok(())
    }

    pub(super) fn header(&self) -> Result<Option<Header>, WireError> {
        let Some(head) = self.rx.get(..HEADER_SIZE) else {
            return Ok(None);
        };
        let object = u32::from_ne_bytes([head[0], head[1], head[2], head[3]]);
        let word = u32::from_ne_bytes([head[4], head[5], head[6], head[7]]);
        let size = (word >> 16) as usize;
        let opcode = (word & 0xffff) as u16;
        if size < HEADER_SIZE || !size.is_multiple_of(4) || size > self.max_message {
            return Err(WireError::Malformed {
                object,
                opcode,
                what: "message size",
            });
        }
        Ok((self.rx.len() >= size).then_some(Header {
            object,
            opcode,
            size,
        }))
    }

    pub(super) fn decode(
        &mut self,
        header: Header,
        signature: &[ArgumentType],
    ) -> Result<Message, WireError> {
        let malformed = |what| WireError::Malformed {
            object: header.object,
            opcode: header.opcode,
            what,
        };
        let mut body = Body {
            bytes: &self.rx[HEADER_SIZE..header.size],
            at: 0,
        };
        let mut args = Vec::with_capacity(signature.len());
        for kind in signature {
            let arg = match kind {
                ArgumentType::Int => {
                    Argument::Int(body.word().ok_or(malformed("short body"))? as i32)
                }
                ArgumentType::Uint => Argument::Uint(body.word().ok_or(malformed("short body"))?),
                ArgumentType::Fixed => {
                    Argument::Fixed(body.word().ok_or(malformed("short body"))? as i32)
                }
                ArgumentType::Object(_) => {
                    Argument::Object(body.word().ok_or(malformed("short body"))?)
                }
                ArgumentType::NewId => {
                    let id = body.word().ok_or(malformed("short body"))?;
                    if id == 0 {
                        return Err(malformed("null new_id"));
                    }
                    Argument::NewId(id)
                }
                ArgumentType::Str(nullable) => {
                    let text = body.string().ok_or(malformed("string"))?;
                    if text.is_none() && *nullable == AllowNull::No {
                        return Err(malformed("null string"));
                    }
                    Argument::Str(text.map(Box::new))
                }
                ArgumentType::Array => {
                    Argument::Array(Box::new(body.array().ok_or(malformed("array"))?.to_vec()))
                }
                ArgumentType::Fd => {
                    Argument::Fd(self.rx_fds.pop_front().ok_or(malformed("missing fd"))?)
                }
            };
            args.push(arg);
        }
        if body.at != body.bytes.len() {
            return Err(malformed("trailing bytes"));
        }
        self.rx.drain(..header.size);
        Ok(Message {
            object: header.object,
            opcode: header.opcode,
            args,
        })
    }

    pub(super) fn push(&mut self, object: u32, opcode: u16, args: Args) -> Result<(), WireError> {
        let start = self.tx.len();
        self.tx.extend_from_slice(&object.to_ne_bytes());
        self.tx.extend_from_slice(&0u32.to_ne_bytes());
        let mut fds = 0;
        for arg in args {
            match arg {
                Argument::Int(value) | Argument::Fixed(value) => {
                    self.tx.extend_from_slice(&value.to_ne_bytes())
                }
                Argument::Uint(value) | Argument::Object(value) | Argument::NewId(value) => {
                    self.tx.extend_from_slice(&value.to_ne_bytes())
                }
                Argument::Str(None) => self.tx.extend_from_slice(&0u32.to_ne_bytes()),
                Argument::Str(Some(text)) => self.put_bytes(text.as_bytes_with_nul()),
                Argument::Array(bytes) => self.put_bytes(&bytes),
                Argument::Fd(fd) => {
                    self.tx_fds.push_back(fd);
                    fds += 1;
                }
            }
        }
        let size = self.tx.len() - start;
        if size > MAX_MESSAGE_SIZE || fds > MAX_FDS_PER_SEND {
            self.tx.truncate(start);
            self.tx_fds.truncate(self.tx_fds.len() - fds);
            return Err(WireError::Backlog(
                "a message larger than the wire format allows",
            ));
        }
        let word = ((size as u32) << 16) | u32::from(opcode);
        self.tx[start + 4..start + 8].copy_from_slice(&word.to_ne_bytes());
        if fds > 0 {
            self.tx_fd_ends.push_back((self.tx.len(), fds));
        }
        if self.tx.len() > TX_BYTE_LIMIT {
            return Err(WireError::Backlog("the peer stopped reading its socket"));
        }
        if self.tx_fds.len() > TX_FD_LIMIT {
            return Err(WireError::Backlog(
                "the peer stopped taking file descriptors",
            ));
        }
        Ok(())
    }

    fn put_bytes(&mut self, bytes: &[u8]) {
        self.tx
            .extend_from_slice(&(bytes.len() as u32).to_ne_bytes());
        self.tx.extend_from_slice(bytes);
        self.tx.resize(self.tx.len().next_multiple_of(4), 0);
    }

    pub(super) fn flush(&mut self) -> Result<(), WireError> {
        while !self.tx.is_empty() {
            let (limit, fd_count) = self.next_send();
            let sent = {
                let fds: Vec<BorrowedFd<'_>> =
                    self.tx_fds.iter().take(fd_count).map(AsFd::as_fd).collect();
                let mut space =
                    [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS_PER_SEND))];
                let mut control = SendAncillaryBuffer::new(&mut space);
                if !fds.is_empty() && !control.push(SendAncillaryMessage::ScmRights(&fds)) {
                    return Err(WireError::Backlog(
                        "file descriptors do not fit one message",
                    ));
                }
                match sendmsg(
                    &self.stream,
                    &[IoSlice::new(&self.tx[..limit])],
                    &mut control,
                    SendFlags::DONTWAIT | SendFlags::NOSIGNAL,
                ) {
                    Ok(sent) => sent,
                    Err(rustix::io::Errno::AGAIN) => return Ok(()),
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(rustix::io::Errno::PIPE) => return Err(WireError::Closed),
                    Err(errno) => return Err(errno.into()),
                }
            };
            self.tx_fds.drain(..fd_count);
            let mut released = 0;
            while released < fd_count {
                let Some((_, count)) = self.tx_fd_ends.pop_front() else {
                    break;
                };
                released += count;
            }
            self.tx.drain(..sent);
            for end in &mut self.tx_fd_ends {
                end.0 = end.0.saturating_sub(sent);
            }
        }
        Ok(())
    }

    fn next_send(&self) -> (usize, usize) {
        let mut fd_count = 0;
        for (index, &(end, count)) in self.tx_fd_ends.iter().enumerate() {
            if fd_count + count > MAX_FDS_PER_SEND {
                let limit = match index.checked_sub(1) {
                    Some(previous) => self.tx_fd_ends[previous].0,
                    None => end,
                };
                return (limit, fd_count);
            }
            fd_count += count;
        }
        (self.tx.len(), fd_count)
    }
}

struct Body<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Body<'a> {
    fn word(&mut self) -> Option<u32> {
        let bytes = self.bytes.get(self.at..self.at + 4)?;
        self.at += 4;
        Some(u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn array(&mut self) -> Option<&'a [u8]> {
        let len = self.word()? as usize;
        let bytes = self.bytes.get(self.at..self.at.checked_add(len)?)?;
        self.at = self.at.checked_add(len.next_multiple_of(4))?;
        (self.at <= self.bytes.len()).then_some(bytes)
    }

    fn string(&mut self) -> Option<Option<CString>> {
        let bytes = self.array()?;
        if bytes.is_empty() {
            return Some(None);
        }
        CString::from_vec_with_nul(bytes.to_vec()).ok().map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wayland_client::backend::protocol::AllowNull;

    fn memfd(content: &[u8]) -> OwnedFd {
        let fd = rustix::fs::memfd_create("eclipse-wire-test", rustix::fs::MemfdFlags::CLOEXEC)
            .expect("memfd");
        rustix::io::write(&fd, content).expect("fill the memfd");
        fd
    }

    fn content(fd: &OwnedFd) -> Vec<u8> {
        let mut bytes = [0u8; 16];
        let read = rustix::io::pread(fd, &mut bytes, 0).expect("read the memfd");
        bytes[..read].to_vec()
    }

    fn decode_all(wire: &mut Wire, signature: &[ArgumentType], count: usize) -> Vec<Message> {
        let mut messages = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while messages.len() < count {
            assert!(
                std::time::Instant::now() < deadline,
                "only {} messages arrived",
                messages.len()
            );
            wire.receive().expect("receive");
            while let Some(header) = wire.header().expect("header") {
                messages.push(wire.decode(header, signature).expect("decode"));
            }
        }
        messages
    }

    #[test]
    fn every_argument_kind_and_its_file_descriptors_cross_the_socket() {
        let (ours, theirs) = UnixStream::pair().expect("socketpair");
        let mut sender = Wire::events(ours).expect("sender");
        let mut receiver = Wire::events(theirs).expect("receiver");
        let signature = [
            ArgumentType::Uint,
            ArgumentType::Int,
            ArgumentType::Fixed,
            ArgumentType::Str(AllowNull::No),
            ArgumentType::Str(AllowNull::Yes),
            ArgumentType::Array,
            ArgumentType::Fd,
            ArgumentType::Object(AllowNull::Yes),
            ArgumentType::NewId,
        ];
        for index in 0..40u32 {
            sender
                .push(
                    7,
                    2,
                    vec![
                        Argument::Uint(index),
                        Argument::Int(-3),
                        Argument::Fixed(256),
                        Argument::Str(Some(Box::new(CString::new("päge").expect("text")))),
                        Argument::Str(None),
                        Argument::Array(Box::new(vec![1, 2, 3])),
                        Argument::Fd(memfd(&index.to_ne_bytes())),
                        Argument::Object(0),
                        Argument::NewId(0xff00_0000 + index),
                    ],
                )
                .expect("encode");
        }
        sender.flush().expect("send");
        assert!(!sender.wants_write(), "40 small messages fit the socket");
        let messages = decode_all(&mut receiver, &signature, 40);
        for (index, message) in (0u32..).zip(&messages) {
            assert_eq!((message.object, message.opcode), (7, 2));
            match message.args.as_slice() {
                [Argument::Uint(sent), Argument::Int(-3), Argument::Fixed(256), Argument::Str(Some(text)), Argument::Str(None), Argument::Array(bytes), Argument::Fd(fd), Argument::Object(0), Argument::NewId(id)] =>
                {
                    assert_eq!(*sent, index);
                    assert_eq!(text.to_str(), Ok("päge"));
                    assert_eq!(**bytes, [1, 2, 3]);
                    assert_eq!(
                        content(fd),
                        index.to_ne_bytes(),
                        "each message keeps its own descriptor although {MAX_FDS_PER_SEND} \
                         travel per send"
                    );
                    assert_eq!(*id, 0xff00_0000 + index);
                }
                other => panic!("message {index} decoded to {other:?}"),
            }
        }
    }

    #[test]
    fn malformed_requests_are_refused_before_anything_is_forwarded() {
        let raw = |words: &[u32]| {
            let (ours, theirs) = UnixStream::pair().expect("socketpair");
            let bytes: Vec<u8> = words.iter().flat_map(|word| word.to_ne_bytes()).collect();
            rustix::io::write(&ours, &bytes).expect("write");
            let mut wire = Wire::requests(theirs).expect("wire");
            wire.receive().expect("receive");
            (ours, wire)
        };
        let header = |size: u32, opcode: u32| (size << 16) | opcode;

        let (_peer, wire) = raw(&[3, header(10, 0), 0, 0]);
        assert!(matches!(
            wire.header(),
            Err(WireError::Malformed {
                what: "message size",
                ..
            })
        ));

        let (_peer, wire) = raw(&[3, header(4100, 0)]);
        assert!(
            matches!(wire.header(), Err(WireError::Malformed { .. })),
            "a request larger than libwayland sends would break the game's connection"
        );

        let (_peer, mut wire) = raw(&[3, header(12, 0), 0]);
        let header_read = wire.header().expect("header").expect("a whole message");
        assert!(matches!(
            wire.decode(header_read, &[ArgumentType::Str(AllowNull::No)]),
            Err(WireError::Malformed {
                what: "null string",
                ..
            })
        ));

        let (_peer, mut wire) = raw(&[3, header(8, 0)]);
        let header_read = wire.header().expect("header").expect("a whole message");
        assert!(matches!(
            wire.decode(header_read, &[ArgumentType::Fd]),
            Err(WireError::Malformed {
                what: "missing fd",
                ..
            })
        ));

        let (_peer, mut wire) = raw(&[3, header(16, 0), 4, 0x6162_6364]);
        let header_read = wire.header().expect("header").expect("a whole message");
        assert!(matches!(
            wire.decode(header_read, &[ArgumentType::Str(AllowNull::No)]),
            Err(WireError::Malformed { what: "string", .. })
        ));
    }
}
