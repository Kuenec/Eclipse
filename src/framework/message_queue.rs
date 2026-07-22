//! Host wait/wake state for `android.os.MessageQueue` native handles.
//!
//! Eclipse pumps Android's main queue from winit, so that one queue must yield when no message is
//! ready. Android worker loopers have the opposite contract: `Looper.loop()` must block until a
//! producer calls `nativeWake`. A distinct handle per queue lets the shared JNI natives preserve
//! both behaviours without baking thread names or app-specific knowledge into the framework.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

const FIRST_HANDLE: i64 = 0x4d51_0000; // "MQ", visibly not a host pointer.

#[derive(Debug)]
struct QueueState {
    is_main: bool,
    wake_pending: Mutex<bool>,
    wake: Condvar,
    waiting: AtomicBool,
}

impl QueueState {
    fn new(is_main: bool) -> Self {
        Self {
            is_main,
            wake_pending: Mutex::new(false),
            wake: Condvar::new(),
            waiting: AtomicBool::new(false),
        }
    }

    fn lock_pending(&self) -> MutexGuard<'_, bool> {
        self.wake_pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn wait(&self, timeout_millis: i32) {
        if timeout_millis == 0 {
            return;
        }

        let mut pending = self.lock_pending();
        if *pending {
            *pending = false;
            return;
        }

        self.waiting.store(true, Ordering::Release);
        if timeout_millis < 0 {
            while !*pending {
                pending = self
                    .wake
                    .wait(pending)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        } else {
            let deadline = Instant::now() + Duration::from_millis(timeout_millis as u64);
            while !*pending {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                let (next, result) = self
                    .wake
                    .wait_timeout(pending, deadline.saturating_duration_since(now))
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                pending = next;
                if result.timed_out() {
                    break;
                }
            }
        }
        if *pending {
            *pending = false;
        }
        self.waiting.store(false, Ordering::Release);
    }

    fn signal(&self) {
        let mut pending = self.lock_pending();
        *pending = true;
        self.wake.notify_one();
    }
}

fn registry() -> &'static Mutex<HashMap<i64, Arc<QueueState>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<i64, Arc<QueueState>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_registry() -> MutexGuard<'static, HashMap<i64, Arc<QueueState>>> {
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn state(handle: i64) -> Option<Arc<QueueState>> {
    lock_registry().get(&handle).cloned()
}

/// Allocate one opaque, never-reused Java `mPtr` handle.
///
/// `None` is the allocation-failure signal expected by `MessageQueue` (`nativeInit` returns zero).
/// Avoiding assertions here matters because release builds use `panic = "abort"` and this function
/// is called from a JNI boundary.
pub(super) fn create(is_main: bool) -> Option<i64> {
    static NEXT_HANDLE: AtomicI64 = AtomicI64::new(FIRST_HANDLE);
    let handle = NEXT_HANDLE
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
            next.checked_add(1)
        })
        .ok()?;
    if handle < FIRST_HANDLE {
        return None;
    }
    match lock_registry().entry(handle) {
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(Arc::new(QueueState::new(is_main)));
            Some(handle)
        }
        std::collections::hash_map::Entry::Occupied(_) => None,
    }
}

/// Implement ATL's boolean `nativePollOnce` convention.
///
/// `true` asks its patched `MessageQueue.next()` to yield `null`. Only the externally pumped main
/// queue does that. A worker waits as Android requires, then returns `false` so Java re-examines its
/// synchronized message list (and observes either the new message, its timeout, or `mQuitting`).
pub(super) fn poll_should_yield(handle: i64, timeout_millis: i32) -> bool {
    let Some(state) = state(handle) else {
        // A stale/destroyed handle must never block an ART thread.
        return timeout_millis != 0;
    };
    if state.is_main {
        return timeout_millis != 0;
    }
    state.wait(timeout_millis);
    false
}

/// Wake a worker poll. The pending bit makes a wake durable across the small pre-wait race window.
pub(super) fn wake(handle: i64) -> bool {
    let Some(state) = state(handle) else {
        return false;
    };
    state.signal();
    true
}

/// Whether the queue is currently inside a blocking worker poll.
pub(super) fn is_idling(handle: i64) -> bool {
    state(handle).is_some_and(|state| state.waiting.load(Ordering::Acquire))
}

/// Retire a Java queue handle and wake any defensive in-flight waiter.
pub(super) fn destroy(handle: i64) -> bool {
    let removed = lock_registry().remove(&handle);
    if let Some(state) = removed {
        state.signal();
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_queue_yields_instead_of_blocking() {
        let handle = create(true).expect("test queue handle");
        assert!(!poll_should_yield(handle, 0));
        assert!(poll_should_yield(handle, -1));
        assert!(poll_should_yield(handle, 25));
        assert!(destroy(handle));
    }

    #[test]
    fn worker_queue_blocks_until_a_durable_wake() {
        let handle = create(false).expect("test queue handle");
        let (sent, received) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            sent.send(poll_should_yield(handle, -1))
                .expect("test receiver must remain alive");
        });

        let deadline = Instant::now() + Duration::from_secs(1);
        while !is_idling(handle) && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(is_idling(handle), "worker never entered its blocking poll");
        assert!(wake(handle));
        assert!(!received
            .recv_timeout(Duration::from_secs(1))
            .expect("wake must release the worker"));
        worker.join().expect("worker must exit cleanly");
        assert!(!is_idling(handle));
        assert!(destroy(handle));
    }

    #[test]
    fn worker_timed_poll_and_destroyed_handle_never_hang() {
        let handle = create(false).expect("test queue handle");
        let started = Instant::now();
        assert!(!poll_should_yield(handle, 5));
        assert!(started.elapsed() >= Duration::from_millis(1));
        assert!(destroy(handle));
        assert!(poll_should_yield(handle, -1));
        assert!(!wake(handle));
    }
}
