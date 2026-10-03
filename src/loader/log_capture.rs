use std::io;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use tracing::subscriber::NoSubscriber;
use tracing::Dispatch;

#[derive(Clone, Default)]
struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

impl io::Write for SharedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn share_callsite_interest_across_threads() {
    static IDLE_DISPATCH: LazyLock<Dispatch> = LazyLock::new(|| Dispatch::new(NoSubscriber::new()));
    LazyLock::force(&IDLE_DISPATCH);
}

pub(crate) fn formatted_log(directives: &str, body: impl FnOnce()) -> String {
    share_callsite_interest_across_threads();
    let buffer = SharedBuffer::default();
    let writer = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(directives))
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .without_time()
        .finish();
    tracing::subscriber::with_default(subscriber, body);
    let bytes = buffer.0.lock().unwrap_or_else(PoisonError::into_inner);
    String::from_utf8_lossy(&bytes).into_owned()
}
