//! Two-level job progress: the host's frame count (total) with the working
//! stage's [`FrameProgress`] (frame) folded in, plus the latest phase-local
//! [`Progress`] as detail.
//!
//! Plugins never see job totals. They report the share of the current call;
//! the host knows how many frames are done and how many there are, so it alone
//! computes the total bar.
use abner_processor_api::{
    Cancellation, FormatPair, Frame, FrameProgress, FrameProgressReporter, Progress,
    ProgressReporter, Result, Session, SpatialInput, VideoFrame,
};
use serde::Serialize;
use std::sync::{Arc, Mutex};

/// Never let a frame still in flight complete its share of the total bar:
/// the last step of a call is followed by delivery, conversion and encoding.
pub const FRAME_CAP: f64 = 0.99;

/// The in-flight call's share, in frames: the working stage's fraction of its
/// call, placed within the chain (`stage` of `stages`) so a frame that moves on
/// to the next stage keeps going forward instead of restarting, times the
/// `frames` the call delivers ([`FrameProgress::frame_count`]). The last of
/// those frames never completes: at most `frames - 1 + FRAME_CAP`.
pub fn frame_share(stage: usize, stages: usize, fraction: f64, frames: u64) -> f64 {
    let stages = stages.max(1);
    let stage = stage.min(stages - 1);
    let frames = frames.max(1) as f64;
    (frames * (stage as f64 + fraction.clamp(0.0, 1.0)) / stages as f64).min(frames - 1.0 + FRAME_CAP)
}

/// Total progress, `0..=1`: whole frames done plus the in-flight call's share
/// (in frames, [`frame_share`]), out of `total`. A finished job (done ≥ total)
/// is exactly 1; an unfinished one never reaches it.
pub fn fold(done: u64, total: u64, frame_share: Option<f64>) -> Option<f64> {
    if total == 0 {
        return None;
    }
    if done >= total {
        return Some(1.0);
    }
    let left = (total - done) as f64;
    let share = frame_share.unwrap_or(0.0).clamp(0.0, left - 1.0 + FRAME_CAP);
    Some((done as f64 + share) / total as f64)
}

/// A snapshot for a CLI line or a GUI bar.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct JobProgress {
    /// Output frames finished (encoded/delivered).
    pub frames_done: u64,
    /// Output frames the job will produce, when known.
    pub frames_total: Option<u64>,
    /// The total bar, `0..=1`: `frames_done` plus the in-flight frame's share,
    /// over `frames_total`. Never decreases during a job.
    pub fraction: Option<f64>,
    /// The frame bar: the working stage's report for its current call. `None`
    /// between calls or when the stage does not report frame progress.
    pub frame: Option<FrameProgress>,
    /// Which stage `frame` (or else `detail`) came from, 0-based.
    pub stage: Option<usize>,
    /// The latest phase-local report from any stage (legacy detail).
    pub detail: Option<Progress>,
}
impl JobProgress {
    /// The frame bar's fraction, `0..=1`.
    pub fn frame_fraction(&self) -> Option<f64> {
        self.frame.as_ref().and_then(FrameProgress::fraction)
    }
}

struct State {
    stages: usize,
    progress: JobProgress,
    sink: Arc<dyn Fn(&JobProgress) + Send + Sync>,
}
impl State {
    fn publish(&mut self) {
        let p = &mut self.progress;
        let share = match (&p.frame, p.stage) {
            (Some(f), Some(stage)) => {
                f.fraction().map(|x| frame_share(stage, self.stages, x, f.frame_count()))
            }
            _ => None,
        };
        if let Some(total) = p.frames_total {
            let next = fold(p.frames_done, total, share);
            p.fraction = match (p.fraction, next) {
                (Some(old), Some(new)) => Some(old.max(new)),
                (_, new) => new,
            };
        }
        (self.sink)(&self.progress);
    }
}

/// Aggregates a job's reports and publishes a [`JobProgress`] on every change.
/// Cheap to clone; the reporters it hands out may be called from any thread.
#[derive(Clone)]
pub struct ProgressTracker(Arc<Mutex<State>>);
impl ProgressTracker {
    /// `sink` runs on whichever thread reported, under the tracker's lock: it
    /// must return promptly and must not call back into the tracker.
    pub fn new(stages: usize, sink: Arc<dyn Fn(&JobProgress) + Send + Sync>) -> Self {
        Self(Arc::new(Mutex::new(State { stages, progress: JobProgress::default(), sink })))
    }
    fn update(&self, f: impl FnOnce(&mut JobProgress)) {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut s.progress);
        s.publish();
    }
    /// Frames finished so far, out of `total`. Ends the in-flight frame.
    pub fn frames(&self, done: u64, total: Option<u64>) {
        self.update(|p| {
            p.frames_done = done;
            p.frames_total = total.map(|t| t.max(done));
            p.frame = None;
        });
    }
    /// A stage's call returned: its frame report no longer describes live work.
    pub fn call_finished(&self, stage: usize) {
        self.update(|p| {
            if p.stage == Some(stage) {
                p.frame = None;
            }
        });
    }
    /// The phase-local reporter for stage `stage`.
    pub fn stage_reporter(&self, stage: usize) -> ProgressReporter {
        let t = self.clone();
        Arc::new(move |progress| {
            t.update(|p| {
                p.detail = Some(progress);
                if p.frame.is_none() {
                    p.stage = Some(stage);
                }
            })
        })
    }
    /// The frame reporter for stage `stage`.
    pub fn frame_reporter(&self, stage: usize) -> FrameProgressReporter {
        let t = self.clone();
        Arc::new(move |frame| {
            t.update(|p| {
                p.frame = Some(frame);
                p.stage = Some(stage);
            })
        })
    }
    pub fn snapshot(&self) -> JobProgress {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).progress.clone()
    }
}

/// Runs `after` whenever a processing call returns (`process`, `process_video`,
/// `process_with_inputs`, `finish`, `finish_video`), so a host can end the
/// frame bar with the call it described: a stale "63/63" never outlives it.
/// Forwards everything else, both reporters included.
pub struct CallBoundary {
    inner: Box<dyn Session>,
    after: Arc<dyn Fn() + Send + Sync>,
}
impl CallBoundary {
    pub fn new(inner: Box<dyn Session>, after: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self { inner, after }
    }
    /// Stage `stage` of a chain reporting to `tracker`: its phase and frame
    /// reporters, and the end of each call.
    pub fn tracked(mut inner: Box<dyn Session>, tracker: &ProgressTracker, stage: usize) -> Self {
        inner.set_progress_reporter(tracker.stage_reporter(stage));
        inner.set_frame_progress_reporter(tracker.frame_reporter(stage));
        let tracker = tracker.clone();
        Self::new(inner, Arc::new(move || tracker.call_finished(stage)))
    }
    fn call<T>(&mut self, f: impl FnOnce(&mut dyn Session) -> T) -> T {
        let out = f(self.inner.as_mut());
        (self.after)();
        out
    }
}
impl Session for CallBoundary {
    fn format_pairs(&self) -> Vec<FormatPair> {
        self.inner.format_pairs()
    }
    fn configure_format(&mut self, pair: FormatPair) -> Result<()> {
        self.inner.configure_format(pair)
    }
    fn process_video(&mut self, input: VideoFrame) -> Result<Vec<VideoFrame>> {
        self.call(|s| s.process_video(input))
    }
    fn finish_video(&mut self) -> Result<Vec<VideoFrame>> {
        self.call(|s| s.finish_video())
    }
    fn set_preview_target(&mut self, pts: i64, time_base: [u32; 2]) {
        self.inner.set_preview_target(pts, time_base)
    }
    fn set_progress_reporter(&mut self, reporter: ProgressReporter) {
        self.inner.set_progress_reporter(reporter)
    }
    fn set_frame_progress_reporter(&mut self, reporter: FrameProgressReporter) {
        self.inner.set_frame_progress_reporter(reporter)
    }
    fn process(&mut self, input: Frame) -> Result<Vec<Frame>> {
        self.call(|s| s.process(input))
    }
    fn process_with_inputs(&mut self, input: Frame, inputs: Vec<SpatialInput>) -> Result<Vec<Frame>> {
        self.call(|s| s.process_with_inputs(input, inputs))
    }
    fn finish(&mut self) -> Result<Vec<Frame>> {
        self.call(|s| s.finish())
    }
    fn output_dimensions(&self) -> [usize; 2] {
        self.inner.output_dimensions()
    }
    fn cancellation(&self) -> Cancellation {
        self.inner.cancellation()
    }
    fn progress(&self) -> Progress {
        self.inner.progress()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tracker(stages: usize) -> (ProgressTracker, Arc<Mutex<Vec<JobProgress>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        (ProgressTracker::new(stages, Arc::new(move |p: &JobProgress| log.lock().unwrap().push(p.clone()))), seen)
    }
    fn step(completed: u64, total: u64) -> FrameProgress {
        FrameProgress { phase: "transformer".into(), completed, total, frames: 0 }
    }

    #[test]
    fn fold_adds_the_frame_share_and_never_completes_an_unfinished_frame() {
        assert_eq!(fold(0, 0, Some(0.5)), None);
        assert_eq!(fold(0, 1, None), Some(0.0));
        assert_eq!(fold(0, 1, Some(0.5)), Some(0.5));
        // The single-frame render that sat at 100% while still working: the
        // last step is not the end of the frame.
        assert_eq!(fold(0, 1, Some(1.0)), Some(FRAME_CAP));
        assert_eq!(fold(3, 4, Some(0.5)), Some(3.5 / 4.0));
        assert_eq!(fold(4, 4, Some(0.5)), Some(1.0));
        assert_eq!(frame_share(0, 2, 1.0, 1), 0.5);
        assert_eq!(frame_share(1, 2, 0.5, 1), 0.75);
        assert_eq!(frame_share(5, 2, 1.0, 1), FRAME_CAP);
        assert_eq!(frame_share(0, 1, 0.5, 0), 0.5, "0 frames means one");
        // A batch call covers its frames; its last frame never completes.
        assert_eq!(frame_share(0, 1, 0.5, 8), 4.0);
        assert_eq!(frame_share(0, 1, 1.0, 8), 7.0 + FRAME_CAP);
        assert_eq!(fold(0, 8, Some(4.0)), Some(0.5));
        assert_eq!(fold(4, 8, Some(7.0 + FRAME_CAP)), Some((7.0 + FRAME_CAP) / 8.0), "capped at what is left");
    }

    #[test]
    fn a_batch_call_moves_the_total_across_its_frames() {
        // Eight frames buffered and rendered in one call: the total follows the
        // call's steps across all eight, not one frame's eighth.
        let (t, _) = tracker(1);
        t.frames(0, Some(16));
        let frame = t.frame_reporter(0);
        frame(FrameProgress { frames: 8, ..step(22, 44) });
        assert_eq!(t.snapshot().fraction, Some(0.25));
        frame(FrameProgress { frames: 8, ..step(44, 44) });
        assert_eq!(t.snapshot().fraction, Some((7.0 + FRAME_CAP) / 16.0));
        t.frames(8, Some(16));
        assert_eq!(t.snapshot().fraction, Some(0.5));
    }

    #[test]
    fn total_moves_during_a_slow_frame_and_frame_bar_resets_per_frame() {
        let (t, _) = tracker(1);
        t.frames(0, Some(4));
        let frame = t.frame_reporter(0);
        frame(step(0, 63));
        assert_eq!(t.snapshot().fraction, Some(0.0));
        frame(step(21, 63));
        let p = t.snapshot();
        assert_eq!(p.frame_fraction(), Some(21.0 / 63.0));
        assert!((p.fraction.unwrap() - (21.0 / 63.0) / 4.0).abs() < 1e-12);
        frame(step(63, 63));
        assert_eq!(t.snapshot().fraction, Some(FRAME_CAP / 4.0));
        t.frames(1, Some(4));
        let p = t.snapshot();
        assert_eq!(p.frame, None, "a delivered frame ends the frame bar");
        assert_eq!(p.fraction, Some(0.25));
        t.frames(4, Some(4));
        assert_eq!(t.snapshot().fraction, Some(1.0));
    }

    #[test]
    fn total_never_goes_backwards() {
        // A buffering stage finishes a call without delivering a frame and the
        // next call starts over at 0: the total bar holds its place.
        let (t, seen) = tracker(1);
        t.frames(0, Some(2));
        let frame = t.frame_reporter(0);
        frame(step(30, 60));
        t.call_finished(0);
        frame(step(0, 60));
        frame(step(15, 60));
        let fractions: Vec<f64> = seen.lock().unwrap().iter().filter_map(|p| p.fraction).collect();
        assert!(fractions.windows(2).all(|w| w[1] >= w[0]), "{fractions:?}");
        assert_eq!(t.snapshot().fraction, Some(0.25));
        assert_eq!(t.snapshot().frame_fraction(), Some(0.25));
    }

    #[test]
    fn stages_split_the_frame_and_phase_reports_stay_detail() {
        let (t, _) = tracker(2);
        t.frames(0, Some(1));
        // A legacy phase that completes says nothing about the frame.
        t.stage_reporter(0)(Progress { phase: "Tiles".into(), completed: 12, total: Some(12) });
        let p = t.snapshot();
        assert_eq!(p.fraction, Some(0.0));
        assert_eq!(p.frame, None);
        assert_eq!(p.stage, Some(0));
        assert_eq!(p.detail.as_ref().unwrap().phase, "Tiles");
        t.frame_reporter(0)(step(10, 10));
        t.call_finished(0);
        assert_eq!(t.snapshot().frame, None);
        t.frame_reporter(1)(step(1, 2));
        let p = t.snapshot();
        assert_eq!(p.stage, Some(1));
        assert_eq!(p.fraction, Some(0.75));
        // Detail from another stage does not steal the frame bar's stage.
        t.stage_reporter(0)(Progress { phase: "x".into(), completed: 0, total: None });
        assert_eq!(t.snapshot().stage, Some(1));
    }

    #[test]
    fn progress_json_shape_is_unchanged() {
        // Serialized API 0.2 reports still parse and round-trip.
        let p: Progress = serde_json::from_str(r#"{"phase":"Tiles","completed":3,"total":12}"#).unwrap();
        assert_eq!(p, Progress { phase: "Tiles".into(), completed: 3, total: Some(12) });
        assert_eq!(serde_json::to_string(&p).unwrap(), r#"{"phase":"Tiles","completed":3,"total":12}"#);
        assert_eq!(p.fraction(), Some(0.25));
        let f: FrameProgress = serde_json::from_str(r#"{"phase":"vae","completed":1,"total":63}"#).unwrap();
        assert_eq!(f.fraction(), Some(1.0 / 63.0));
        assert_eq!(FrameProgress { total: 0, ..f.clone() }.fraction(), None);
        assert_eq!(f.frame_count(), 1);
        assert_eq!(serde_json::to_string(&f).unwrap(), r#"{"phase":"vae","completed":1,"total":63}"#);
        let b: FrameProgress = serde_json::from_str(r#"{"phase":"vae","completed":1,"total":63,"frames":8}"#).unwrap();
        assert_eq!(b.frame_count(), 8);
    }
}
