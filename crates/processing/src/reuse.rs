//! Sessions kept between jobs. A processor that can [`Session::reset`] is
//! created once per settings and size instead of once per kept range of an
//! export or per preview render: for one that loads a model on creation,
//! that load is most of what a Render press costs.
use abner_processor_api::{
    Cancellation, Frame, FormatPair, FrameProgressReporter, Progress, ProgressReporter, Result,
    Session, SpatialInput, VideoFrame,
};
use std::{cell::RefCell, rc::Rc};

type Kept = Rc<RefCell<Box<dyn Session>>>;

/// A kept session lent to one job: the job's wrappers own the `Lent`, the
/// [`Pool`] keeps the session for the next job.
struct Lent(Kept);
impl Session for Lent {
    fn format_pairs(&self) -> Vec<FormatPair> {
        self.0.borrow().format_pairs()
    }
    fn configure_format(&mut self, pair: FormatPair) -> Result<()> {
        self.0.borrow_mut().configure_format(pair)
    }
    fn process_video(&mut self, input: VideoFrame) -> Result<Vec<VideoFrame>> {
        self.0.borrow_mut().process_video(input)
    }
    fn finish_video(&mut self) -> Result<Vec<VideoFrame>> {
        self.0.borrow_mut().finish_video()
    }
    fn set_preview_target(&mut self, pts: i64, time_base: [u32; 2]) {
        self.0.borrow_mut().set_preview_target(pts, time_base)
    }
    fn set_progress_reporter(&mut self, reporter: ProgressReporter) {
        self.0.borrow_mut().set_progress_reporter(reporter)
    }
    fn set_frame_progress_reporter(&mut self, reporter: FrameProgressReporter) {
        self.0.borrow_mut().set_frame_progress_reporter(reporter)
    }
    fn process(&mut self, input: Frame) -> Result<Vec<Frame>> {
        self.0.borrow_mut().process(input)
    }
    fn process_with_inputs(&mut self, input: Frame, inputs: Vec<SpatialInput>) -> Result<Vec<Frame>> {
        self.0.borrow_mut().process_with_inputs(input, inputs)
    }
    fn finish(&mut self) -> Result<Vec<Frame>> {
        self.0.borrow_mut().finish()
    }
    fn reset(&mut self) -> bool {
        self.0.borrow_mut().reset()
    }
    fn output_dimensions(&self) -> [usize; 2] {
        self.0.borrow().output_dimensions()
    }
    fn cancellation(&self) -> Cancellation {
        self.0.borrow().cancellation()
    }
    fn progress(&self) -> Progress {
        self.0.borrow().progress()
    }
}

/// The sessions the last job used, keyed by what made them (processor,
/// settings, input size). Lives on the thread that runs the sessions.
#[derive(Default)]
pub struct Pool {
    kept: Vec<(String, Kept)>,
    used: Vec<(String, Kept)>,
}
impl Pool {
    /// A session for `key`: the kept one, reset for a new sequence, or a new
    /// one from `create` when there is none or it cannot reset.
    pub fn take(
        &mut self,
        key: String,
        create: impl FnOnce() -> Result<Box<dyn Session>>,
    ) -> Result<Box<dyn Session>> {
        let kept = self
            .kept
            .iter()
            .position(|(k, _)| *k == key)
            .map(|i| self.kept.remove(i).1)
            .filter(|s| {
                let mut s = s.borrow_mut();
                s.reset() && s.cancellation().check().is_ok()
            });
        let session = match kept {
            Some(s) => s,
            None => Rc::new(RefCell::new(create()?)),
        };
        self.used.push((key, session.clone()));
        Ok(Box::new(Lent(session)))
    }
    /// A job ended: keep the sessions it used for the next one (dropping any
    /// it did not ask for), or none, after a failure that may have left them
    /// in a state `reset` was never meant to recover from.
    pub fn settle(&mut self, keep: bool) {
        self.kept = std::mem::take(&mut self.used);
        if !keep {
            self.kept.clear();
        }
    }
    /// Drop every kept session (the sources changed, or an export needs the
    /// memory).
    pub fn clear(&mut self) {
        self.kept.clear();
        self.used.clear();
    }
    pub fn len(&self) -> usize {
        self.kept.len()
    }
    pub fn is_empty(&self) -> bool {
        self.kept.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct Counting {
        resets: Arc<AtomicUsize>,
        resettable: bool,
        cancel: Cancellation,
    }
    impl Session for Counting {
        fn process(&mut self, input: Frame) -> Result<Vec<Frame>> {
            Ok(vec![input])
        }
        fn reset(&mut self) -> bool {
            self.resets.fetch_add(1, Ordering::Relaxed);
            self.cancel = Cancellation::default();
            self.resettable
        }
        fn output_dimensions(&self) -> [usize; 2] {
            [2, 2]
        }
        fn cancellation(&self) -> Cancellation {
            self.cancel.clone()
        }
        fn progress(&self) -> Progress {
            Progress::default()
        }
    }
    #[test]
    fn kept_sessions_are_reset_and_reused_by_key() {
        let (created, resets) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let make = |resettable: bool| {
            let (created, resets) = (created.clone(), resets.clone());
            move || -> Result<Box<dyn Session>> {
                created.fetch_add(1, Ordering::Relaxed);
                Ok(Box::new(Counting { resets, resettable, cancel: Cancellation::default() }))
            }
        };
        let mut pool = Pool::default();
        for _ in 0..3 {
            let a = pool.take("a".into(), make(true)).unwrap();
            // A cancelled job leaves the token raised: reset must replace it.
            a.cancellation().cancel();
            drop(pool.take("b".into(), make(false)).unwrap());
            drop(a);
            pool.settle(true);
        }
        // "a" made once and reset twice; "b" cannot reset, so made each time.
        assert_eq!(created.load(Ordering::Relaxed), 1 + 3);
        assert_eq!(resets.load(Ordering::Relaxed), 2 + 2);
        assert_eq!(pool.len(), 2);
        // A different key (other settings) drops what the job did not use.
        drop(pool.take("c".into(), make(true)).unwrap());
        pool.settle(true);
        assert_eq!(pool.len(), 1);
        pool.settle(false);
        assert!(pool.is_empty());
    }
}
