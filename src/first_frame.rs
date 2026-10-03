use std::sync::{Condvar, Mutex, PoisonError};

static ENGINE: FirstFrame = FirstFrame::new();

pub(crate) fn presented() {
    ENGINE.presented();
}

pub fn wait() {
    ENGINE.wait();
}

pub fn shown() -> bool {
    ENGINE.shown()
}

struct FirstFrame {
    presented: Mutex<bool>,
    shown: Condvar,
}

impl FirstFrame {
    const fn new() -> Self {
        Self {
            presented: Mutex::new(false),
            shown: Condvar::new(),
        }
    }

    fn presented(&self) {
        let mut presented = self
            .presented
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !*presented {
            *presented = true;
            self.shown.notify_all();
        }
    }

    fn shown(&self) -> bool {
        *self
            .presented
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn wait(&self) {
        let mut presented = self
            .presented
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while !*presented {
            presented = self
                .shown
                .wait(presented)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::FirstFrame;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn wait_returns_once_presented_from_another_thread() {
        let frame = FirstFrame::new();
        let (sender, waited) = mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                frame.wait();
                sender.send(()).unwrap();
            });
            assert_eq!(
                waited.recv_timeout(Duration::from_millis(200)),
                Err(mpsc::RecvTimeoutError::Timeout),
                "nothing was presented yet"
            );
            frame.presented();
            waited
                .recv_timeout(Duration::from_secs(10))
                .expect("the waiter wakes at the first present");
        });
    }

    #[test]
    fn presented_is_idempotent() {
        let frame = FirstFrame::new();
        frame.presented();
        frame.presented();
        frame.wait();
        frame.wait();
        assert!(frame.shown());
    }

    #[test]
    fn shown_reports_a_present_without_waiting() {
        let frame = FirstFrame::new();
        assert!(!frame.shown());
        frame.presented();
        assert!(frame.shown());
    }
}
