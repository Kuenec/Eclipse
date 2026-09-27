use std::fmt;
use std::sync::mpsc;
use std::time::Duration;

use raw_window_handle::RawDisplayHandle;

const LOAD_TIMEOUT: Duration = Duration::from_millis(500);

const WAYLAND_QUEUED_COMMANDS: usize = 8;

pub(crate) enum HostClipboard {
    Wayland(WaylandClipboard),
    X11(Box<x11_clipboard::Clipboard>),
}

#[derive(Debug)]
pub(crate) enum ClipboardError {
    UnsupportedDisplay,
    WaylandWorker(std::io::Error),
    WaylandWorkerStopped,
    WaylandOwnerUnresponsive,
    Wayland(std::io::Error),
    X11(x11_clipboard::error::Error),
    NotUtf8(std::string::FromUtf8Error),
}

impl fmt::Display for ClipboardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedDisplay => {
                f.write_str("the host display is neither Wayland nor X11, so it has no clipboard")
            }
            Self::WaylandWorker(e) => {
                write!(f, "the Wayland clipboard worker thread could not start: {e}")
            }
            Self::WaylandWorkerStopped => f.write_str("the Wayland clipboard worker has stopped"),
            Self::WaylandOwnerUnresponsive => write!(
                f,
                "the application holding the Wayland clipboard did not answer a paste within {LOAD_TIMEOUT:?}"
            ),
            Self::Wayland(e) => write!(f, "Wayland clipboard transfer failed: {e}"),
            Self::X11(e) => write!(f, "X11 clipboard transfer failed: {e}"),
            Self::NotUtf8(e) => write!(f, "the clipboard text is not UTF-8: {e}"),
        }
    }
}

impl std::error::Error for ClipboardError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::UnsupportedDisplay
            | Self::WaylandWorkerStopped
            | Self::WaylandOwnerUnresponsive => None,
            Self::WaylandWorker(e) | Self::Wayland(e) => Some(e),
            Self::X11(e) => Some(e),
            Self::NotUtf8(e) => Some(e),
        }
    }
}

impl HostClipboard {
    pub(crate) unsafe fn for_display_that_outlives_it(
        display: RawDisplayHandle,
    ) -> Result<Self, ClipboardError> {
        match display {
            RawDisplayHandle::Wayland(wayland) => {
                let clipboard =
                    unsafe { smithay_clipboard::Clipboard::new(wayland.display.as_ptr()) };
                WaylandClipboard::spawn(move |received| {
                    serve_wayland_clipboard(&clipboard, received)
                })
                .map(Self::Wayland)
            }
            RawDisplayHandle::Xlib(_) | RawDisplayHandle::Xcb(_) => x11_clipboard::Clipboard::new()
                .map(|clipboard| Self::X11(Box::new(clipboard)))
                .map_err(ClipboardError::X11),
            _ => Err(ClipboardError::UnsupportedDisplay),
        }
    }

    pub(crate) fn load(&mut self) -> Result<String, ClipboardError> {
        match self {
            Self::Wayland(clipboard) => clipboard.load(),
            Self::X11(clipboard) => {
                let atoms = &clipboard.getter.atoms;
                let bytes = clipboard
                    .load(
                        atoms.clipboard,
                        atoms.utf8_string,
                        atoms.property,
                        LOAD_TIMEOUT,
                    )
                    .map_err(ClipboardError::X11)?;
                String::from_utf8(bytes).map_err(ClipboardError::NotUtf8)
            }
        }
    }

    pub(crate) fn store(&self, text: String) -> Result<(), ClipboardError> {
        match self {
            Self::Wayland(clipboard) => clipboard.store(text),
            Self::X11(clipboard) => {
                let atoms = &clipboard.setter.atoms;
                clipboard
                    .store(atoms.clipboard, atoms.utf8_string, text)
                    .map_err(ClipboardError::X11)
            }
        }
    }
}

enum WaylandCommand {
    Load(mpsc::SyncSender<std::io::Result<String>>),
    Store(String),
    Stop,
}

pub(crate) struct WaylandClipboard {
    commands: mpsc::SyncSender<WaylandCommand>,
    unanswered_load: Option<mpsc::Receiver<std::io::Result<String>>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

fn serve_wayland_clipboard(
    clipboard: &smithay_clipboard::Clipboard,
    received: mpsc::Receiver<WaylandCommand>,
) {
    for command in received {
        match command {
            WaylandCommand::Load(reply) => {
                if reply.send(clipboard.load()).is_err() {
                    tracing::debug!(
                        "a paste stopped waiting before the Wayland clipboard answered"
                    );
                }
            }
            WaylandCommand::Store(text) => clipboard.store(text),
            WaylandCommand::Stop => return,
        }
    }
}

impl WaylandClipboard {
    fn spawn(
        serve: impl FnOnce(mpsc::Receiver<WaylandCommand>) + Send + 'static,
    ) -> Result<Self, ClipboardError> {
        let (commands, received) = mpsc::sync_channel(WAYLAND_QUEUED_COMMANDS);
        let worker = std::thread::Builder::new()
            .name("eclipse-clipboard".to_owned())
            .spawn(move || serve(received))
            .map_err(ClipboardError::WaylandWorker)?;
        Ok(Self {
            commands,
            unanswered_load: None,
            worker: Some(worker),
        })
    }

    fn earlier_load_unanswered(&self) -> bool {
        self.unanswered_load
            .as_ref()
            .is_some_and(|earlier| matches!(earlier.try_recv(), Err(mpsc::TryRecvError::Empty)))
    }

    fn send(&self, command: WaylandCommand) -> Result<(), ClipboardError> {
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => ClipboardError::WaylandOwnerUnresponsive,
                mpsc::TrySendError::Disconnected(_) => ClipboardError::WaylandWorkerStopped,
            })
    }

    fn load(&mut self) -> Result<String, ClipboardError> {
        if self.earlier_load_unanswered() {
            return Err(ClipboardError::WaylandOwnerUnresponsive);
        }
        let (reply, answer) = mpsc::sync_channel(1);
        self.send(WaylandCommand::Load(reply))?;
        let answered = answer.recv_timeout(LOAD_TIMEOUT);
        self.unanswered_load =
            matches!(answered, Err(mpsc::RecvTimeoutError::Timeout)).then_some(answer);
        match answered {
            Ok(loaded) => loaded.map_err(ClipboardError::Wayland),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(ClipboardError::WaylandOwnerUnresponsive),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(ClipboardError::WaylandWorkerStopped),
        }
    }

    fn store(&self, text: String) -> Result<(), ClipboardError> {
        self.send(WaylandCommand::Store(text))
    }
}

impl Drop for WaylandClipboard {
    fn drop(&mut self) {
        if self.earlier_load_unanswered() {
            tracing::warn!(
                "waiting for the application holding the Wayland clipboard to answer an earlier paste"
            );
        }
        if self.commands.send(WaylandCommand::Stop).is_err() {
            return;
        }
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                tracing::error!("the Wayland clipboard worker panicked while stopping");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clipboard_owner(
        answer: impl Fn(usize) -> Option<&'static str> + Send + 'static,
    ) -> (WaylandClipboard, mpsc::Receiver<String>) {
        let (seen, requests) = mpsc::channel();
        let clipboard = WaylandClipboard::spawn(move |received| {
            let mut unanswered = Vec::new();
            let mut loads = 0;
            for command in received {
                match command {
                    WaylandCommand::Load(reply) => {
                        match answer(loads) {
                            Some(text) => reply.send(Ok(text.to_owned())).unwrap(),
                            None => unanswered.push(reply),
                        }
                        loads += 1;
                        seen.send("load".to_owned()).unwrap();
                    }
                    WaylandCommand::Store(text) => seen.send(format!("store {text}")).unwrap(),
                    WaylandCommand::Stop => return,
                }
            }
        })
        .unwrap();
        (clipboard, requests)
    }

    #[test]
    fn a_wayland_paste_gives_up_when_the_clipboard_owner_never_answers() {
        let (mut clipboard, requests) = clipboard_owner(|_| None);

        let started = std::time::Instant::now();
        assert!(matches!(
            clipboard.load(),
            Err(ClipboardError::WaylandOwnerUnresponsive)
        ));
        assert!(started.elapsed() >= LOAD_TIMEOUT);
        assert!(matches!(
            clipboard.load(),
            Err(ClipboardError::WaylandOwnerUnresponsive)
        ));
        clipboard.store("copied".to_owned()).unwrap();
        drop(clipboard);

        assert_eq!(
            requests.try_iter().collect::<Vec<_>>(),
            ["load", "store copied"]
        );
    }

    #[test]
    fn a_wayland_paste_after_a_late_answer_reads_the_clipboard_again() {
        let (release, released) = mpsc::channel::<()>();
        let released = std::sync::Mutex::new(released);
        let (mut clipboard, requests) = clipboard_owner(move |load| match load {
            0 => {
                released.lock().unwrap().recv().unwrap();
                Some("late")
            }
            _ => Some("fresh"),
        });

        assert!(matches!(
            clipboard.load(),
            Err(ClipboardError::WaylandOwnerUnresponsive)
        ));
        release.send(()).unwrap();
        assert_eq!(requests.recv().unwrap(), "load");
        assert_eq!(clipboard.load().unwrap(), "fresh");
    }
}
