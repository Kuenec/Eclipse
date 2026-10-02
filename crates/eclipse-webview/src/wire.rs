use eclipse_webview::proto::{self, ConsumerMsg, ProtoError};
use gtk4::gio::{self, prelude::*};
use gtk4::glib;
use std::cell::RefCell;
use std::fmt;
use std::os::fd::OwnedFd;

const READ_CHUNK: usize = 64 * 1024;

pub(crate) const BATCH_LIMIT: usize = 64;

#[derive(Debug)]
pub(crate) enum WireError {
    Socket(glib::Error),
    Protocol(ProtoError),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Socket(error) => write!(f, "control socket: {error}"),
            Self::Protocol(error) => write!(f, "control stream: {error}"),
        }
    }
}

impl From<glib::Error> for WireError {
    fn from(error: glib::Error) -> Self {
        Self::Socket(error)
    }
}

impl From<ProtoError> for WireError {
    fn from(error: ProtoError) -> Self {
        Self::Protocol(error)
    }
}

pub(crate) enum Inbound {
    Messages(Vec<ConsumerMsg>),
    Closed,
}

pub(crate) struct Wire {
    socket: gio::Socket,
    inbox: RefCell<Vec<u8>>,
}

impl Wire {
    pub(crate) fn new(fd: OwnedFd) -> Result<Self, WireError> {
        Ok(Self {
            socket: gio::Socket::from_fd(fd)?,
            inbox: RefCell::new(Vec::new()),
        })
    }

    pub(crate) fn socket(&self) -> &gio::Socket {
        &self.socket
    }

    pub(crate) fn send(&self, frame: &[u8]) -> Result<(), glib::Error> {
        let mut sent = 0;
        while sent < frame.len() {
            sent +=
                self.socket
                    .send_with_blocking(&frame[sent..], true, None::<&gio::Cancellable>)?;
        }
        Ok(())
    }

    pub(crate) fn receive(&self) -> Result<Inbound, WireError> {
        let mut inbox = self.inbox.borrow_mut();
        let mut messages = Vec::new();
        let mut chunk = vec![0u8; READ_CHUNK];
        loop {
            take_frames(&mut inbox, &mut messages)?;
            if messages.len() >= BATCH_LIMIT {
                return Ok(Inbound::Messages(messages));
            }
            match self
                .socket
                .receive_with_blocking(&mut chunk, false, None::<&gio::Cancellable>)
            {
                Ok(0) if inbox.is_empty() && messages.is_empty() => return Ok(Inbound::Closed),
                Ok(0) if inbox.is_empty() => return Ok(Inbound::Messages(messages)),
                Ok(0) => return Err(WireError::Protocol(ProtoError::Truncated)),
                Ok(read) => inbox.extend_from_slice(&chunk[..read]),
                Err(error) if error.matches(gio::IOErrorEnum::WouldBlock) => {
                    return Ok(Inbound::Messages(messages));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

fn take_frames(inbox: &mut Vec<u8>, messages: &mut Vec<ConsumerMsg>) -> Result<(), ProtoError> {
    let mut consumed = 0;
    while messages.len() < BATCH_LIMIT {
        let pending = &inbox[consumed..];
        let Some(length) = proto::consumer_frame_len(pending)? else {
            break;
        };
        messages.push(proto::read_consumer_msg(&mut &pending[..length])?);
        consumed += length;
    }
    inbox.drain(..consumed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_frames_wait_in_the_inbox_until_they_are_whole() {
        let first = ConsumerMsg::GoBack { view: 1 }.encode().expect("encode");
        let second = ConsumerMsg::Reload { view: 2 }.encode().expect("encode");
        let mut inbox = first.clone();
        inbox.extend_from_slice(&second[..3]);
        let mut messages = Vec::new();
        take_frames(&mut inbox, &mut messages).expect("frames");
        assert_eq!(messages, vec![ConsumerMsg::GoBack { view: 1 }]);
        assert_eq!(inbox, second[..3]);
        inbox.extend_from_slice(&second[3..]);
        take_frames(&mut inbox, &mut messages).expect("frames");
        assert_eq!(
            messages,
            vec![
                ConsumerMsg::GoBack { view: 1 },
                ConsumerMsg::Reload { view: 2 }
            ]
        );
        assert!(inbox.is_empty());
    }

    #[test]
    fn a_hostile_length_is_rejected_before_it_is_buffered() {
        let mut inbox = (proto::GLOBAL_FRAME_CAP + 1).to_le_bytes().to_vec();
        let mut messages = Vec::new();
        assert!(matches!(
            take_frames(&mut inbox, &mut messages),
            Err(ProtoError::Oversized { .. })
        ));
    }

    #[test]
    fn frames_are_taken_in_bounded_batches() {
        let frame = ConsumerMsg::StopLoading { view: 3 }
            .encode()
            .expect("encode");
        let mut inbox = frame.repeat(BATCH_LIMIT + 5);
        let mut messages = Vec::new();
        take_frames(&mut inbox, &mut messages).expect("frames");
        assert_eq!(messages.len(), BATCH_LIMIT);
        assert_eq!(inbox.len(), frame.len() * 5);
    }
}
