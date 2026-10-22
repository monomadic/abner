//! Coalesce worker notifications until the render loop services a redraw.
//!
//! Based on Switchblade's Waker. Arming and the first wake share a lock so
//! their startup race cannot send two events for the same pending batch.
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone)]
pub(crate) struct Waker {
    inner: Arc<Inner>,
}

struct Inner {
    pending: AtomicBool,
    notify: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl Waker {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                pending: AtomicBool::new(false),
                notify: Mutex::new(None),
            }),
        }
    }

    /// Retain work before arming; otherwise send once per redraw batch.
    pub(crate) fn wake(&self) {
        if self.inner.pending.load(Ordering::Acquire) {
            return;
        }
        let notify = self.inner.notify.lock().unwrap();
        if !self.inner.pending.swap(true, Ordering::AcqRel)
            && let Some(f) = &*notify
        {
            f();
        }
    }

    /// Install the event-loop proxy after GPU initialization. The callback
    /// only sends an event; it must not call back into this handle.
    pub(crate) fn arm(&self, f: impl Fn() + Send + 'static) {
        let mut notify = self.inner.notify.lock().unwrap();
        assert!(notify.is_none(), "waker already armed");
        *notify = Some(Box::new(f));
        if self.inner.pending.load(Ordering::Acquire) {
            notify.as_ref().unwrap()();
        }
    }

    /// Clear at the start of a redraw, before consuming any delivered work.
    pub(crate) fn take_pending(&self) -> bool {
        self.inner.pending.swap(false, Ordering::AcqRel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Barrier, atomic::AtomicUsize};

    fn arm_counter(waker: &Waker) -> Arc<AtomicUsize> {
        let hits = Arc::new(AtomicUsize::new(0));
        let count = hits.clone();
        waker.arm(move || {
            count.fetch_add(1, Ordering::SeqCst);
        });
        hits
    }

    #[test]
    fn player_clones_coalesce_concurrent_notifications() {
        let waker = Waker::new();
        let hits = arm_counter(&waker);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let player = waker.clone();
                scope.spawn(move || {
                    for _ in 0..100 {
                        player.wake();
                    }
                });
            }
        });
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn redraw_clears_latch_before_next_completion() {
        let waker = Waker::new();
        let hits = arm_counter(&waker);
        assert!(!waker.take_pending());
        // Each paused step / completed seek can wake an otherwise idle loop.
        for expected in 1..=3 {
            waker.wake();
            waker.wake();
            assert_eq!(hits.load(Ordering::SeqCst), expected);
            assert!(waker.take_pending());
            assert!(!waker.take_pending());
        }
    }

    #[test]
    fn completion_during_redraw_requests_another_frame() {
        let waker = Waker::new();
        let hits = arm_counter(&waker);
        waker.wake();
        assert!(waker.take_pending()); // start redraw, before consuming work
        let worker = waker.clone();
        std::thread::spawn(move || worker.wake()).join().unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert!(waker.take_pending());
    }

    #[test]
    fn startup_completion_is_replayed_once_when_armed() {
        let waker = Waker::new();
        waker.wake();
        waker.wake();
        let hits = arm_counter(&waker);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        waker.wake();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert!(waker.take_pending());
        waker.wake();
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn arming_without_work_waits_for_completion() {
        let waker = Waker::new();
        let hits = arm_counter(&waker);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        waker.wake();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn concurrent_arm_and_wake_send_exactly_once() {
        for _ in 0..128 {
            let waker = Waker::new();
            let barrier = Arc::new(Barrier::new(2));
            let worker = waker.clone();
            let start = barrier.clone();
            let thread = std::thread::spawn(move || {
                start.wait();
                worker.wake();
            });
            barrier.wait();
            let hits = arm_counter(&waker);
            thread.join().unwrap();
            assert_eq!(hits.load(Ordering::SeqCst), 1);
            assert!(waker.take_pending());
        }
    }
}
