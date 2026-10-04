use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PresentPace {
    Unpaced,
    Every(Duration),
}

static ENGINE: Pacer = Pacer::new();

pub(crate) fn set_pace(pace: PresentPace) {
    ENGINE.set_pace(pace);
}

pub(crate) fn wait_for_turn() {
    ENGINE.wait_for_turn();
}

struct Pacer {
    paced: AtomicBool,
    turn: Mutex<Turn>,
    pace_changed: Condvar,
}

struct Turn {
    pace: PresentPace,
    last_present: Option<Instant>,
}

impl Pacer {
    const fn new() -> Self {
        Self {
            paced: AtomicBool::new(false),
            turn: Mutex::new(Turn {
                pace: PresentPace::Unpaced,
                last_present: None,
            }),
            pace_changed: Condvar::new(),
        }
    }

    fn set_pace(&self, pace: PresentPace) {
        let mut turn = self.lock();
        turn.pace = pace;
        self.paced
            .store(pace != PresentPace::Unpaced, Ordering::Relaxed);
        drop(turn);
        self.pace_changed.notify_all();
    }

    fn wait_for_turn(&self) {
        if !self.paced.load(Ordering::Relaxed) {
            return;
        }
        let mut turn = self.lock();
        loop {
            let now = Instant::now();
            let due = next_due(turn.pace, turn.last_present, now);
            if due <= now {
                break;
            }
            turn = self
                .pace_changed
                .wait_timeout(turn, due - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        turn.last_present = Some(Instant::now());
    }

    fn lock(&self) -> MutexGuard<'_, Turn> {
        self.turn.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn next_due(pace: PresentPace, last_present: Option<Instant>, now: Instant) -> Instant {
    match (pace, last_present) {
        (PresentPace::Every(interval), Some(last)) => last + interval,
        (PresentPace::Every(_), None) | (PresentPace::Unpaced, _) => now,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn a_present_is_due_one_interval_after_the_last_one() {
        let now = Instant::now();
        let last = now - Duration::from_millis(50);
        let every = PresentPace::Every(Duration::from_millis(200));
        assert_eq!(
            next_due(every, Some(last), now),
            last + Duration::from_millis(200)
        );
        assert_eq!(next_due(every, None, now), now);
        assert_eq!(next_due(PresentPace::Unpaced, Some(last), now), now);
        let long_ago = now - Duration::from_secs(5);
        assert!(next_due(every, Some(long_ago), now) < now);
    }

    #[test]
    fn ten_presents_at_20_ms_take_at_least_180_ms() {
        let pacer = Pacer::new();
        pacer.set_pace(PresentPace::Every(Duration::from_millis(20)));
        let started = Instant::now();
        for _ in 0..10 {
            pacer.wait_for_turn();
        }
        assert!(
            started.elapsed() >= Duration::from_millis(180),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn an_unpaced_present_never_waits() {
        let pacer = Pacer::new();
        pacer.set_pace(PresentPace::Every(Duration::from_secs(10)));
        pacer.wait_for_turn();
        pacer.set_pace(PresentPace::Unpaced);
        let started = Instant::now();
        for _ in 0..1000 {
            pacer.wait_for_turn();
        }
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn unpacing_releases_a_waiting_present() {
        let pacer = Arc::new(Pacer::new());
        pacer.set_pace(PresentPace::Every(Duration::from_secs(10)));
        pacer.wait_for_turn();
        let returned = Arc::new(AtomicBool::new(false));
        let waiter = {
            let pacer = Arc::clone(&pacer);
            let returned = Arc::clone(&returned);
            std::thread::spawn(move || {
                pacer.wait_for_turn();
                returned.store(true, Ordering::SeqCst);
                Instant::now()
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !returned.load(Ordering::SeqCst),
            "the present waited for its turn"
        );
        let unpaced = Instant::now();
        pacer.set_pace(PresentPace::Unpaced);
        let resumed = waiter.join().expect("the waiting present returns");
        assert!(
            resumed.duration_since(unpaced) < Duration::from_millis(500),
            "{:?}",
            resumed.duration_since(unpaced)
        );
    }
}
