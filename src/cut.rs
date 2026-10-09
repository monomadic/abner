//! Cut mode's model: a keyframe index, a list of removed ranges over ONE
//! clip's source time, and the lossless export of what is left.
//!
//! The edit lives in source time — cuts are ranges taken out, nothing is
//! ever shifted — so the timeline, the player and the export all read the
//! same numbers. Snapping is to keyframes by default: a kept segment that
//! starts on one can be stream-copied, so the export re-encodes nothing.
//! The in point snaps BACK and the out point FORWARD (the removed range
//! grows to whole GOPs); `warning` says what that costs.
//!
//! Everything that reads the file (the packet scan, the waveform, the
//! subtitle track) runs on its own worker and lands through a receiver
//! drained by `poll`, so entering the mode never blocks the loop.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};

/// Waveform resolution: peaks per second of audio.
pub const WAVE_HZ: f64 = 20.0;
/// Sample rate the waveform worker asks ffmpeg for; a multiple of `WAVE_HZ`.
const WAVE_RATE: usize = 4000;
/// Two times closer than this are the same boundary.
const EPS: f64 = 1e-3;

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Snap {
    /// In/out land on keyframes: the export is a stream copy.
    Keyframe,
    /// In/out stay where they were asked: the export re-encodes.
    Frame,
}

#[derive(Debug, Clone, PartialEq, Default)]
struct Edit {
    /// Removed ranges, sorted, never touching or overlapping.
    cuts: Vec<(f64, f64)>,
    /// Extra boundaries inside kept material (`S`).
    splits: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
    pub cut: bool,
    /// 1-based among its own kind: "Keep 2", "Cut 1".
    pub number: usize,
}

#[derive(Debug, Clone)]
pub struct Chapter {
    pub start: f64,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct Cue {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

struct Scan {
    keys: Vec<f64>,
    chapters: Vec<Chapter>,
    facts: Facts,
}

/// What the file's streams say about themselves, for the inspector's
/// Streams tab and the chip strip over the chapter list. Rows are
/// `(key, value)` pairs already formatted the way the panel shows them.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    /// `H.264 · High 5.1`, or empty until the scan lands.
    pub video_title: String,
    pub video: Vec<(&'static str, String)>,
    /// `AAC LC`; empty when the file has no audio.
    pub audio_title: String,
    pub audio: Vec<(&'static str, String)>,
    /// `SRT`, `MOV text`; empty when the file has no subtitle stream.
    pub subs_title: String,
    /// The short strip: `H.264`, `2160p`, `23.976`, `AAC 2ch`, `SRT`.
    pub chips: Vec<String>,
}

pub struct Cut {
    pub path: PathBuf,
    pub duration: f64,
    /// Keyframe times of the first video stream, sorted. Empty until the
    /// scan lands (`scanning`), and snapping is the identity until then.
    pub keys: Vec<f64>,
    pub chapters: Vec<Chapter>,
    /// Chapters were added here, so an export has something to write even
    /// with nothing cut.
    pub chapters_edited: bool,
    pub facts: Facts,
    pub cues: Vec<Cue>,
    /// One peak (0–255) per `1 / WAVE_HZ` seconds of the first audio stream.
    pub wave: Vec<u8>,
    pub in_req: Option<f64>,
    pub out_req: Option<f64>,
    pub snap: Snap,
    /// Timeline view: the source time at the lanes' left edge, and the
    /// scale in logical px per second. 0 px/s = not fitted yet.
    pub t0: f64,
    pub pps: f64,
    pub status: String,
    edit: Edit,
    undo: Vec<Edit>,
    scan: Option<Receiver<Result<Scan, String>>>,
    cue_rx: Option<Receiver<Vec<Cue>>>,
    wave_rx: Option<Receiver<Vec<u8>>>,
    export: Option<Receiver<Result<String, String>>>,
    /// Stops the waveform worker's ffmpeg when the mode is dropped.
    cancel: Arc<AtomicBool>,
}

impl Drop for Cut {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Cut {
    /// The model alone, no workers: what the tests drive.
    pub fn new(path: &Path, duration: f64) -> Self {
        Self {
            path: path.to_path_buf(),
            duration,
            keys: Vec::new(),
            chapters: Vec::new(),
            chapters_edited: false,
            facts: Facts::default(),
            cues: Vec::new(),
            wave: Vec::new(),
            in_req: None,
            out_req: None,
            snap: Snap::Keyframe,
            t0: 0.0,
            pps: 0.0,
            status: String::new(),
            edit: Edit::default(),
            undo: Vec::new(),
            scan: None,
            cue_rx: None,
            wave_rx: None,
            export: None,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The model plus its three readers, each on a worker.
    pub fn open(path: &Path, duration: f64) -> Self {
        let mut cut = Self::new(path, duration);
        let (tx, rx) = channel();
        let p = path.to_path_buf();
        std::thread::spawn(move || {
            let _ = tx.send(scan(&p).map_err(|e| e.to_string()));
        });
        cut.scan = Some(rx);
        let (tx, rx) = channel();
        let p = path.to_path_buf();
        std::thread::spawn(move || {
            let _ = tx.send(read_cues(&p));
        });
        cut.cue_rx = Some(rx);
        let (tx, rx) = channel();
        let p = path.to_path_buf();
        let cancel = cut.cancel.clone();
        std::thread::spawn(move || {
            read_wave(&p, &cancel, &tx);
        });
        cut.wave_rx = Some(rx);
        cut
    }

    pub fn scanning(&self) -> bool {
        self.scan.is_some()
    }

    pub fn reading_wave(&self) -> bool {
        self.wave_rx.is_some()
    }

    pub fn exporting(&self) -> bool {
        self.export.is_some()
    }

    /// Take whatever the workers have finished.
    pub fn poll(&mut self) {
        fn take<T>(slot: &mut Option<Receiver<T>>) -> Option<Option<T>> {
            match slot.as_ref()?.try_recv() {
                Ok(v) => { *slot = None; Some(Some(v)) }
                Err(TryRecvError::Disconnected) => { *slot = None; Some(None) }
                Err(TryRecvError::Empty) => None,
            }
        }
        match take(&mut self.scan) {
            Some(Some(Ok(scan))) => {
                self.chapters = scan.chapters;
                self.facts = scan.facts;
                self.set_keys(scan.keys);
            }
            Some(Some(Err(e))) => {
                log::error!("keyframe scan failed: {e}");
                self.status = format!("Keyframe scan failed: {e}");
            }
            Some(None) => self.status = "Keyframe scan failed: worker stopped".into(),
            None => {}
        }
        if let Some(Some(cues)) = take(&mut self.cue_rx) {
            self.cues = cues;
        }
        // The waveform arrives in batches, so it fills in left to right while
        // a long file's audio is still being decoded. The lane says "reading"
        // only until the worker ends.
        if let Some(rx) = &self.wave_rx {
            loop {
                match rx.try_recv() {
                    Ok(mut batch) => self.wave.append(&mut batch),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => { self.wave_rx = None; break; }
                }
            }
        }
        match take(&mut self.export) {
            Some(Some(Ok(done))) => self.status = format!("Exported {done}"),
            Some(Some(Err(e))) => {
                log::error!("cut export failed: {e}");
                self.status = format!("Export failed: {e}");
            }
            Some(None) => self.status = "Export failed: worker stopped".into(),
            None => {}
        }
    }

    pub fn set_keys(&mut self, mut keys: Vec<f64>) {
        keys.sort_by(f64::total_cmp);
        keys.dedup_by(|a, b| (*a - *b).abs() < EPS);
        self.keys = keys;
    }

    /// The keyframe at or before `t` (the start of its GOP).
    pub fn prev_key(&self, t: f64) -> f64 {
        let i = self.keys.partition_point(|k| *k <= t + EPS);
        if i == 0 { 0.0 } else { self.keys[i - 1] }
    }

    /// The keyframe at or after `t`; the end of the clip past the last one.
    pub fn next_key(&self, t: f64) -> f64 {
        let i = self.keys.partition_point(|k| *k < t - EPS);
        self.keys.get(i).copied().unwrap_or(self.duration)
    }

    /// The keyframe strictly before / after `t`, for ⇧← / ⇧→.
    pub fn key_step(&self, t: f64, dir: i32) -> f64 {
        if dir > 0 {
            let i = self.keys.partition_point(|k| *k <= t + EPS);
            self.keys.get(i).copied().unwrap_or(t)
        } else {
            let i = self.keys.partition_point(|k| *k < t - EPS);
            if i == 0 { 0.0 } else { self.keys[i - 1] }
        }
    }

    /// Start a chapter at `t`. Refused within half a second of one that is
    /// already there. Returns its (1-based) number.
    pub fn add_chapter(&mut self, t: f64) -> Option<usize> {
        let t = t.clamp(0.0, self.duration);
        if self.chapters.iter().any(|c| (c.start - t).abs() < 0.5) {
            return None;
        }
        let at = self.chapters.partition_point(|c| c.start < t);
        let mut n = self.chapters.len() + 1;
        while self.chapters.iter().any(|c| c.title == format!("Chapter {n}")) { n += 1; }
        self.chapters.insert(at, Chapter { start: t, title: format!("Chapter {n}") });
        self.chapters_edited = true;
        Some(at + 1)
    }

    pub fn on_key(&self, t: f64, frame: f64) -> bool {
        !self.keys.is_empty() && (t - self.prev_key(t + frame * 0.5)).abs() < frame * 0.5
    }

    fn snap_in(&self, t: f64) -> f64 {
        if self.snap == Snap::Frame || self.keys.is_empty() { t } else { self.prev_key(t) }
    }

    fn snap_out(&self, t: f64) -> f64 {
        if self.snap == Snap::Frame || self.keys.is_empty() { t } else { self.next_key(t) }
    }

    /// Where a split asked for at `t` would land.
    pub fn split_point(&self, t: f64) -> f64 {
        self.snap_in(t)
    }

    pub fn in_snapped(&self) -> Option<f64> {
        self.in_req.map(|t| self.snap_in(t))
    }

    pub fn out_snapped(&self) -> Option<f64> {
        self.out_req.map(|t| self.snap_out(t))
    }

    /// The range `X` would remove: both points set, snapped, non-empty.
    pub fn selection(&self) -> Option<(f64, f64)> {
        let (a, b) = (self.in_snapped()?, self.out_snapped()?);
        (b - a > EPS).then_some((a, b))
    }

    pub fn set_in(&mut self, t: f64) {
        self.in_req = Some(t.clamp(0.0, self.duration));
        if self.out_req.is_some_and(|o| o <= t) {
            self.out_req = None;
        }
    }

    pub fn set_out(&mut self, t: f64) {
        self.out_req = Some(t.clamp(0.0, self.duration));
        if self.in_req.is_some_and(|i| i >= t) {
            self.in_req = None;
        }
    }

    pub fn clear_selection(&mut self) {
        self.in_req = None;
        self.out_req = None;
    }

    /// The selection is exactly a range already cut: `X` puts it back.
    pub fn selection_is_cut(&self) -> bool {
        self.selection().is_some_and(|(a, b)| {
            self.edit.cuts.iter().any(|c| (c.0 - a).abs() < EPS && (c.1 - b).abs() < EPS)
        })
    }

    /// Remove the selection — or restore it, when it names an existing cut.
    pub fn cut_selection(&mut self) -> bool {
        let Some((a, b)) = self.selection() else { return false };
        self.undo.push(self.edit.clone());
        if self.selection_is_cut() {
            self.edit.cuts.retain(|c| !((c.0 - a).abs() < EPS && (c.1 - b).abs() < EPS));
        } else {
            let (mut a, mut b) = (a, b);
            // Absorb everything the new range touches.
            self.edit.cuts.retain(|c| {
                let touches = c.0 <= b + EPS && c.1 >= a - EPS;
                if touches {
                    a = a.min(c.0);
                    b = b.max(c.1);
                }
                !touches
            });
            self.edit.cuts.push((a, b));
            self.edit.cuts.sort_by(|x, y| x.0.total_cmp(&y.0));
            self.edit.splits.retain(|s| *s <= a + EPS || *s >= b - EPS);
        }
        self.clear_selection();
        true
    }

    /// A boundary at `t` (snapped like an in point). False when it would
    /// land on an existing boundary or inside removed material.
    pub fn split(&mut self, t: f64) -> bool {
        let t = self.snap_in(t);
        if t < EPS || t > self.duration - EPS || self.boundaries().iter().any(|b| (b - t).abs() < EPS) {
            return false;
        }
        if self.edit.cuts.iter().any(|c| t > c.0 && t < c.1) {
            return false;
        }
        self.undo.push(self.edit.clone());
        self.edit.splits.push(t);
        self.edit.splits.sort_by(f64::total_cmp);
        true
    }

    /// Where a dragged edge lands: the nearest keyframe, or as asked.
    fn snap_near(&self, t: f64) -> f64 {
        if self.snap == Snap::Frame || self.keys.is_empty() {
            return t;
        }
        let (a, b) = (self.prev_key(t), self.next_key(t));
        if t - a <= b - t { a } else { b }
    }

    /// Every boundary a pointer can take hold of: the ends of the kept
    /// pieces, which are the clip's own ends, the cuts' edges and the splits.
    pub fn edges(&self) -> Vec<f64> {
        self.boundaries()
    }

    /// One undo step for a whole drag: call once when an edge is grabbed.
    pub fn begin_trim(&mut self) {
        self.undo.push(self.edit.clone());
    }

    /// Drag the boundary at `edge` to `to` (snapped to the nearest
    /// keyframe), and return where it now is. A cut's edge resizes the cut
    /// (down to nothing, which removes it); a split slides; the clip's own
    /// start or end trims a new cut in from that side. An edge never
    /// crosses its neighbours.
    pub fn move_edge(&mut self, edge: f64, to: f64) -> f64 {
        let bounds = self.boundaries();
        let Some(i) = bounds.iter().position(|b| (b - edge).abs() < EPS) else { return edge };
        let lo = if i == 0 { 0.0 } else { bounds[i - 1] };
        let hi = bounds.get(i + 1).copied().unwrap_or(self.duration);
        let to = self.snap_near(to.clamp(lo, hi)).clamp(lo, hi);
        if let Some(c) = self.edit.cuts.iter_mut().find(|c| (c.1 - edge).abs() < EPS) {
            // The far edge of the cut on the left: this one may also pull
            // back INTO the cut, down to its start.
            c.1 = to.max(c.0);
            let to = c.1;
            self.edit.cuts.retain(|c| c.1 - c.0 > EPS);
            return to;
        }
        if let Some(c) = self.edit.cuts.iter_mut().find(|c| (c.0 - edge).abs() < EPS) {
            c.0 = to.min(c.1);
            let to = c.0;
            self.edit.cuts.retain(|c| c.1 - c.0 > EPS);
            return to;
        }
        if let Some(s) = self.edit.splits.iter_mut().find(|s| (**s - edge).abs() < EPS) {
            // A split dragged onto a neighbour is that neighbour: drop it.
            *s = to;
            self.edit.splits.retain(|s| *s - lo > EPS && hi - *s > EPS);
            return to;
        }
        if i == 0 && to > EPS {
            self.edit.cuts.insert(0, (0.0, to));
        } else if i + 1 == bounds.len() && self.duration - to > EPS {
            self.edit.cuts.push((to, self.duration));
        }
        // A trim that ran up against the next cut is one cut with it.
        let mut merged: Vec<(f64, f64)> = Vec::new();
        for c in self.edit.cuts.drain(..) {
            match merged.last_mut() {
                Some(last) if c.0 - last.1 < EPS => last.1 = c.1,
                _ => merged.push(c),
            }
        }
        self.edit.cuts = merged;
        to
    }

    pub fn undo(&mut self) -> bool {
        match self.undo.pop() {
            Some(edit) => { self.edit = edit; true }
            None => false,
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn cuts(&self) -> &[(f64, f64)] {
        &self.edit.cuts
    }

    #[cfg(test)]
    pub fn splits(&self) -> &[f64] {
        &self.edit.splits
    }

    fn boundaries(&self) -> Vec<f64> {
        let mut b = vec![0.0, self.duration];
        for c in &self.edit.cuts {
            b.push(c.0);
            b.push(c.1);
        }
        b.extend(&self.edit.splits);
        b.sort_by(f64::total_cmp);
        b.dedup_by(|a, b| (*a - *b).abs() < EPS);
        b
    }

    /// The clip, start to end, as alternating kept and removed pieces.
    pub fn segments(&self) -> Vec<Segment> {
        let (mut keeps, mut cuts) = (0, 0);
        self.boundaries()
            .windows(2)
            .map(|w| {
                let mid = (w[0] + w[1]) / 2.0;
                let cut = self.edit.cuts.iter().any(|c| mid > c.0 && mid < c.1);
                let n = if cut { &mut cuts } else { &mut keeps };
                *n += 1;
                Segment { start: w[0], end: w[1], cut, number: *n }
            })
            .collect()
    }

    /// The kept ranges with splits ignored: what the export writes.
    pub fn keeps(&self) -> Vec<(f64, f64)> {
        let mut out = Vec::new();
        let mut at = 0.0;
        for c in &self.edit.cuts {
            if c.0 - at > EPS {
                out.push((at, c.0));
            }
            at = c.1;
        }
        if self.duration - at > EPS {
            out.push((at, self.duration));
        }
        out
    }

    pub fn kept(&self) -> f64 {
        self.keeps().iter().map(|k| k.1 - k.0).sum()
    }

    /// While playing: `t` is inside removed material, resume here.
    pub fn skip(&self, t: f64) -> Option<f64> {
        self.edit.cuts.iter().find(|c| t >= c.0 - EPS && t < c.1 - EPS).map(|c| c.1)
    }

    /// What keyframe snapping does to the selection that the user did not
    /// ask for: the extra material it removes at each end.
    pub fn warning(&self) -> Option<String> {
        if self.snap == Snap::Frame || self.keys.is_empty() {
            return None;
        }
        let extra_out = self.out_req.map(|t| self.snap_out(t) - t).unwrap_or(0.0);
        let extra_in = self.in_req.map(|t| t - self.snap_in(t)).unwrap_or(0.0);
        if extra_out >= extra_in && extra_out > 0.5 {
            let t = self.out_req?;
            let gop = self.next_key(t) - self.prev_key(t);
            Some(format!("Out sits inside a {gop:.1} s GOP — snapping also cuts {extra_out:.2} s you meant to keep."))
        } else if extra_in > 0.5 {
            let t = self.in_req?;
            let gop = self.next_key(t + EPS * 2.0) - self.prev_key(t);
            Some(format!("In sits inside a {gop:.1} s GOP — snapping also cuts {extra_in:.2} s you meant to keep."))
        } else {
            None
        }
    }

    /// The keyframes inside `[a, b]`.
    /// Keyframe count, median and longest GOP in seconds, once the scan
    /// has landed.
    pub fn gop_stats(&self) -> Option<(usize, f64, f64)> {
        if self.keys.len() < 2 { return None; }
        let mut gaps: Vec<f64> = self.keys.windows(2).map(|w| w[1] - w[0]).collect();
        gaps.sort_by(f64::total_cmp);
        Some((self.keys.len(), gaps[gaps.len() / 2], gaps[gaps.len() - 1]))
    }

    pub fn keys_in(&self, a: f64, b: f64) -> &[f64] {
        let lo = self.keys.partition_point(|k| *k < a);
        let hi = self.keys.partition_point(|k| *k <= b);
        &self.keys[lo..hi]
    }

    /// The loudest waveform peak (0..1) between two times.
    pub fn peak(&self, a: f64, b: f64) -> Option<f32> {
        let lo = ((a * WAVE_HZ) as usize).min(self.wave.len());
        let hi = ((b * WAVE_HZ).ceil() as usize).clamp(lo, self.wave.len());
        self.wave[lo..hi.max((lo + 1).min(self.wave.len()))].iter().max().map(|p| *p as f32 / 255.0)
    }

    // ---- timeline view ----

    /// Seconds the lanes show at the current scale.
    pub fn span(&self, width: f32) -> f64 {
        width as f64 / self.pps.max(1e-6)
    }

    /// Fit the first view (the design's 18 px/s, or the whole clip when
    /// that is shorter than the lanes) and keep `t0` inside the clip.
    pub fn fit_view(&mut self, width: f32) {
        let whole = width as f64 / self.duration.max(0.1);
        if self.pps <= 0.0 {
            self.pps = whole.max(18.0);
        }
        self.pps = self.pps.clamp(whole, whole.max(600.0));
        self.t0 = self.t0.clamp(0.0, (self.duration - self.span(width)).max(0.0));
    }

    pub fn pan(&mut self, seconds: f64, width: f32) {
        self.t0 += seconds;
        self.fit_view(width);
    }

    /// Scale by `factor`, holding the time under `anchor` (px from the
    /// lanes' left edge) where it is.
    pub fn zoom(&mut self, factor: f64, anchor: f32, width: f32) {
        let t = self.t0 + anchor as f64 / self.pps.max(1e-6);
        self.pps *= factor;
        self.fit_view(width);
        self.t0 = t - anchor as f64 / self.pps;
        self.fit_view(width);
    }

    /// Where the scale sits between "the whole clip" (0) and the closest
    /// zoom (1), on a log scale — the zoom slider's position.
    pub fn zoom_fraction(&self, width: f32) -> f64 {
        let whole = width as f64 / self.duration.max(0.1);
        let range = (whole.max(600.0) / whole).ln();
        if range <= 0.0 { 0.0 } else { ((self.pps / whole).ln() / range).clamp(0.0, 1.0) }
    }

    pub fn set_zoom_fraction(&mut self, f: f64, anchor: f32, width: f32) {
        let whole = width as f64 / self.duration.max(0.1);
        let target = whole * (whole.max(600.0) / whole).powf(f.clamp(0.0, 1.0));
        self.zoom(target / self.pps.max(1e-6), anchor, width);
    }

    /// Page the view so `t` is on screen.
    pub fn reveal(&mut self, t: f64, width: f32) {
        let span = self.span(width);
        if t < self.t0 || t > self.t0 + span {
            self.t0 = t - span * 0.1;
            self.fit_view(width);
        }
    }

    // ---- export ----

    pub fn output_path(&self) -> PathBuf {
        let stem = self.path.file_stem().unwrap_or_default().to_string_lossy();
        let ext = match self.path.extension().map(|e| e.to_string_lossy().to_lowercase()) {
            Some(e) if matches!(e.as_str(), "mp4" | "mov" | "mkv" | "m4v") => e,
            _ => "mkv".into(),
        };
        self.path.with_file_name(format!("{stem}.cut.{ext}"))
    }

    /// Write the kept ranges beside the source (`clip.cut.mp4`).
    pub fn start_export(&mut self) {
        if self.export.is_some() {
            return;
        }
        let keeps = self.keeps();
        if (self.edit.cuts.is_empty() && !self.chapters_edited) || keeps.is_empty() {
            self.status = "Nothing to export: cut something or add a chapter first".into();
            return;
        }
        let (source, dest, snap) = (self.path.clone(), self.output_path(), self.snap);
        let chapters = self.chapters.clone();
        let name = dest.file_name().unwrap_or_default().to_string_lossy().into_owned();
        self.status = format!("Exporting {name}…");
        let (tx, rx) = channel();
        self.export = Some(rx);
        std::thread::spawn(move || {
            let how = if snap == Snap::Keyframe { "stream copy" } else { "re-encoded" };
            let _ = tx.send(
                export(&source, &dest, &keeps, snap, &chapters)
                    .map(|()| format!("{name} ({} segments, {how})", keeps.len()))
                    .map_err(|e| e.to_string()),
            );
        });
    }
}

/// Keyframes (the first video stream's packets flagged `K`) and chapters.
/// Reading packets is an index walk for MP4/MOV and a demux for the rest;
/// either way nothing is decoded.
fn scan(path: &Path) -> anyhow::Result<Scan> {
    use anyhow::Context;
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", "packet=pts_time,flags", "-of", "csv=p=0"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .context("running ffprobe (is ffmpeg installed?)")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("ffprobe: {}", err.lines().last().unwrap_or("failed").trim());
    }
    let keys = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let (pts, flags) = l.split_once(',')?;
            flags.starts_with('K').then(|| pts.trim().parse::<f64>().ok()).flatten()
        })
        .collect();
    let mut chapters = Vec::new();
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_chapters", "-of", "json"])
        .arg(path)
        .stdin(Stdio::null())
        .output();
    if let Ok(out) = out
        && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&out.stdout)
        && let Some(list) = json["chapters"].as_array()
    {
        for (i, c) in list.iter().enumerate() {
            let Some(start) = c["start_time"].as_str().and_then(|s| s.parse().ok()) else { continue };
            let title = c["tags"]["title"].as_str().map(str::to_string).unwrap_or_else(|| format!("Chapter {}", i + 1));
            chapters.push(Chapter { start, title });
        }
    }
    let facts = Command::new("ffprobe")
        .args(["-v", "error", "-show_format", "-show_streams", "-of", "json"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .ok()
        .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
        .map(|v| parse_facts(&v))
        .unwrap_or_default();
    Ok(Scan { keys, chapters, facts })
}

fn codec_name(raw: &str) -> String {
    match raw {
        "h264" => "H.264".into(),
        "hevc" => "H.265".into(),
        "prores" => "ProRes".into(),
        "av1" => "AV1".into(),
        "vp9" => "VP9".into(),
        "aac" => "AAC".into(),
        "mp3" => "MP3".into(),
        "opus" => "Opus".into(),
        "subrip" => "SRT".into(),
        "mov_text" => "MOV text".into(),
        "ass" => "ASS".into(),
        other => other.to_uppercase(),
    }
}

/// ffprobe's `-show_streams -show_format` JSON as the inspector's rows.
pub fn parse_facts(v: &serde_json::Value) -> Facts {
    let streams = v["streams"].as_array().cloned().unwrap_or_default();
    let first = |kind: &str| streams.iter().find(|s| s["codec_type"].as_str() == Some(kind));
    let num = |s: &serde_json::Value, k: &str| s[k].as_str().and_then(|x| x.parse::<f64>().ok()).or_else(|| s[k].as_f64());
    let rate = |s: &serde_json::Value| {
        let r = s["avg_frame_rate"].as_str().unwrap_or("0/1");
        let (n, d) = r.split_once('/').unwrap_or((r, "1"));
        match (n.parse::<f64>(), d.parse::<f64>()) { (Ok(n), Ok(d)) if d > 0.0 && n > 0.0 => Some(n / d), _ => None }
    };
    let mbps = |b: f64| if b >= 1e6 { format!("{:.1} Mb/s", b / 1e6) } else { format!("{:.0} kb/s", b / 1e3) };
    let mut f = Facts::default();
    if let Some(s) = first("video") {
        let codec = codec_name(s["codec_name"].as_str().unwrap_or("?"));
        let level = s["level"].as_i64().filter(|l| *l > 0).map(|l| if l >= 10 && l % 10 != 0 || l >= 10 { format!("{}.{}", l / 10, l % 10) } else { l.to_string() });
        let profile = s["profile"].as_str().map(|p| p.to_string());
        f.video_title = [Some(codec.clone()), profile, level].into_iter().flatten().collect::<Vec<_>>().join(" · ").replacen(" · ", " · ", 1);
        let (w, h) = (num(s, "width").unwrap_or(0.0) as u32, num(s, "height").unwrap_or(0.0) as u32);
        let fps = rate(s);
        let pix = s["pix_fmt"].as_str().unwrap_or("");
        let depth = if pix.contains("12") { 12 } else if pix.contains("10") { 10 } else { 8 };
        let chroma = if pix.contains("444") { "4:4:4" } else if pix.contains("422") { "4:2:2" } else { "4:2:0" };
        f.video.push(("Size", format!("{w} × {h}")));
        if let Some(fps) = fps { f.video.push(("Rate", format!("{fps:.3} fps"))); }
        f.video.push(("Depth", format!("{depth}-bit · {chroma}")));
        let matrix = s["color_space"].as_str().filter(|c| *c != "unknown").map(|c| c.to_uppercase());
        let range = s["color_range"].as_str().map(|r| r.to_string());
        if matrix.is_some() || range.is_some() {
            f.video.push(("Colour", [matrix, range].into_iter().flatten().collect::<Vec<_>>().join(" · ")));
        }
        if let Some(b) = num(s, "bit_rate").or_else(|| num(&v["format"], "bit_rate")) {
            f.video.push(("Bitrate", mbps(b)));
        }
        f.chips.push(codec);
        if h > 0 { f.chips.push(format!("{h}p")); }
        if let Some(fps) = fps { f.chips.push(trim_rate(fps)); }
    }
    if let Some(s) = first("audio") {
        let codec = codec_name(s["codec_name"].as_str().unwrap_or("?"));
        f.audio_title = [Some(codec.clone()), s["profile"].as_str().map(|p| p.to_string())].into_iter().flatten().collect::<Vec<_>>().join(" ");
        let ch = num(s, "channels").unwrap_or(0.0) as u32;
        let layout = s["channel_layout"].as_str().unwrap_or("");
        f.audio.push(("Channels", if layout.is_empty() { ch.to_string() } else { format!("{ch} · {layout}") }));
        let hz = num(s, "sample_rate").unwrap_or(0.0);
        let mut rate_row = format!("{} kHz", (hz / 100.0).round() / 10.0);
        if let Some(b) = num(s, "bit_rate") { rate_row = format!("{rate_row} · {}", mbps(b)); }
        f.audio.push(("Rate", rate_row));
        f.chips.push(format!("{codec} {ch}ch"));
    }
    if let Some(s) = first("subtitle") {
        f.subs_title = codec_name(s["codec_name"].as_str().unwrap_or("?"));
        f.chips.push(f.subs_title.clone());
    }
    f
}

/// 23.976 stays 23.976; 25.000 is just 25.
fn trim_rate(fps: f64) -> String {
    let s = format!("{fps:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The first subtitle stream as cues. A clip with none (or with bitmap
/// subtitles, which have no text to show) yields nothing.
fn read_cues(path: &Path) -> Vec<Cue> {
    let out = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0:s:0", "-f", "srt", "-"])
        .stdin(Stdio::null())
        .output();
    match out {
        Ok(out) if out.status.success() => parse_srt(&String::from_utf8_lossy(&out.stdout)),
        _ => Vec::new(),
    }
}

fn parse_srt(text: &str) -> Vec<Cue> {
    fn stamp(s: &str) -> Option<f64> {
        let (hms, ms) = s.trim().split_once([',', '.'])?;
        let mut parts = hms.split(':').map(|p| p.parse::<f64>().ok());
        let (h, m, s) = (parts.next()??, parts.next()??, parts.next()??);
        Some(h * 3600.0 + m * 60.0 + s + ms.parse::<f64>().ok()? / 1000.0)
    }
    let mut cues = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let Some((a, b)) = line.split_once("-->") else { continue };
        let (Some(start), Some(end)) = (stamp(a), stamp(b.split_whitespace().next().unwrap_or(""))) else { continue };
        let body: Vec<&str> = lines.by_ref().take_while(|l| !l.trim().is_empty()).collect();
        let mut text = String::new();
        let mut tag = false;
        // SRT carries <i>/<font> markup; the lane shows plain text.
        for ch in body.join(" ").chars() {
            match ch {
                '<' => tag = true,
                '>' if tag => tag = false,
                c if !tag => text.push(c),
                _ => {}
            }
        }
        cues.push(Cue { start, end, text: text.trim().to_string() });
    }
    cues
}

/// The first audio stream as one peak per `1 / WAVE_HZ` s. Decodes the
/// whole track (mono, 4 kHz), streaming: memory is the peaks alone.
fn read_wave(path: &Path, cancel: &AtomicBool, tx: &std::sync::mpsc::Sender<Vec<u8>>) {
    let child = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args(["-vn", "-map", "0:a:0", "-ac", "1", "-ar", &WAVE_RATE.to_string(), "-f", "s16le", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return };
    let Some(mut pipe) = child.stdout.take() else { return };
    let bucket = WAVE_RATE / WAVE_HZ as usize;
    let mut peaks: Vec<u8> = Vec::new();
    let (mut peak, mut count) = (0u16, 0usize);
    let mut buf = [0u8; 1 << 15];
    let mut odd: Option<u8> = None;
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            break;
        }
        let n = match pipe.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        // Ship what has been read so far, ten seconds of peaks at a time.
        if peaks.len() >= (WAVE_HZ as usize) * 10 && tx.send(std::mem::take(&mut peaks)).is_err() {
            let _ = child.kill();
            break;
        }
        let mut bytes = buf[..n].iter().copied();
        loop {
            let lo = match odd.take() {
                Some(b) => b,
                None => match bytes.next() { Some(b) => b, None => break },
            };
            let Some(hi) = bytes.next() else { odd = Some(lo); break };
            peak = peak.max(i16::from_le_bytes([lo, hi]).unsigned_abs());
            count += 1;
            if count == bucket {
                peaks.push((peak >> 7).min(255) as u8);
                (peak, count) = (0, 0);
            }
        }
    }
    let _ = child.wait();
    if !peaks.is_empty() {
        let _ = tx.send(peaks);
    }
}

/// Write `keeps` (source-time ranges) of `source` to `dest`, through a
/// temporary beside it that is renamed only on success.
///
/// `Snap::Keyframe` is a stream copy through the concat demuxer — one
/// `inpoint`/`outpoint` entry per range, valid because every range starts
/// on a keyframe. `Snap::Frame` selects the frames inside the ranges and
/// re-encodes (H.264 CRF 16 + AAC), since a range that starts between
/// keyframes has nothing to copy its first frames from.
/// The chapters an export carries, on the OUTPUT's clock: a chapter that
/// starts inside a kept range moves up by what was cut before it; one that
/// starts inside a cut is dropped. Each runs to the next start (the last to
/// the end of the output). `(start, end, title)`.
pub fn output_chapters(chapters: &[Chapter], keeps: &[(f64, f64)]) -> Vec<(f64, f64, String)> {
    let mut before = 0.0;
    let mut starts: Vec<(f64, String)> = Vec::new();
    for &(a, b) in keeps {
        for c in chapters.iter().filter(|c| c.start >= a - EPS && c.start < b - EPS) {
            starts.push((before + (c.start - a), c.title.clone()));
        }
        before += b - a;
    }
    // A cut can swallow a chapter's start while its body survives: that
    // body becomes the start of its kept remainder, titled as it was.
    let total = before;
    starts.iter().enumerate()
        .map(|(i, (s, t))| (*s, starts.get(i + 1).map_or(total, |n| n.0), t.clone()))
        .collect()
}

fn ffmetadata(chapters: &[(f64, f64, String)]) -> String {
    let mut text = String::from(";FFMETADATA1\n");
    for (a, b, title) in chapters {
        let title = title.replace('\\', "\\\\").replace('=', "\\=").replace(';', "\\;").replace('#', "\\#").replace('\n', " ");
        text.push_str(&format!("[CHAPTER]\nTIMEBASE=1/1000\nSTART={}\nEND={}\ntitle={title}\n", (a * 1000.0).round() as i64, (b * 1000.0).round() as i64));
    }
    text
}

pub fn export(source: &Path, dest: &Path, keeps: &[(f64, f64)], snap: Snap, chapters: &[Chapter]) -> anyhow::Result<()> {
    use anyhow::Context;
    let nonce = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let ext = dest.extension().unwrap_or_default().to_string_lossy().into_owned();
    let tag = format!("{}.{nonce}.tmp", std::process::id());
    let temporary = dest.with_extension(format!("{tag}.{ext}"));
    let list = dest.with_extension(format!("{tag}.txt"));
    let meta = dest.with_extension(format!("{tag}.ffmeta"));
    let carried = output_chapters(chapters, keeps);
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-nostdin", "-y", "-v", "error"]);
    match snap {
        Snap::Keyframe => {
            let source = source.canonicalize().unwrap_or_else(|_| source.to_path_buf());
            let quoted = source.to_string_lossy().replace('\'', "'\\''");
            let mut text = String::from("ffconcat version 1.0\n");
            for (a, b) in keeps {
                text.push_str(&format!("file '{quoted}'\ninpoint {a:.6}\noutpoint {b:.6}\n"));
            }
            std::fs::write(&list, text).context("writing the segment list")?;
            cmd.args(["-f", "concat", "-safe", "0", "-i"]).arg(&list);
            if !carried.is_empty() {
                std::fs::write(&meta, ffmetadata(&carried)).context("writing the chapter list")?;
                cmd.arg("-i").arg(&meta);
            }
            cmd.args(["-map", "0:v:0", "-map", "0:a?", "-c", "copy", "-avoid_negative_ts", "make_zero"]);
            if !carried.is_empty() { cmd.args(["-map_metadata", "0", "-map_chapters", "1"]); }
        }
        Snap::Frame => {
            let expr = keeps.iter().map(|(a, b)| format!("gte(t,{a:.6})*lt(t,{b:.6})")).collect::<Vec<_>>().join("+");
            cmd.arg("-i").arg(source);
            if !carried.is_empty() {
                std::fs::write(&meta, ffmetadata(&carried)).context("writing the chapter list")?;
                cmd.arg("-i").arg(&meta);
                cmd.args(["-map_metadata", "0", "-map_chapters", "1"]);
            }
            cmd.args(["-map", "0:v:0", "-map", "0:a?"]);
            cmd.args(["-vf", &format!("select='{expr}',setpts=N/FRAME_RATE/TB")]);
            cmd.args(["-af", &format!("aselect='{expr}',asetpts=N/SR/TB")]);
            cmd.args(["-c:v", "libx264", "-crf", "16", "-preset", "medium", "-pix_fmt", "yuv420p"]);
            cmd.args(["-c:a", "aac", "-b:a", "192k"]);
        }
    }
    let out = cmd.arg(&temporary).stdin(Stdio::null()).output().context("running ffmpeg (is ffmpeg installed?)");
    let _ = std::fs::remove_file(&list);
    let _ = std::fs::remove_file(&meta);
    let out = out?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&temporary);
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("ffmpeg: {}", err.lines().last().unwrap_or("failed").trim());
    }
    if let Err(e) = std::fs::rename(&temporary, dest) {
        let _ = std::fs::remove_file(&temporary);
        return Err(e.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 60 s with a keyframe every 2 s, and one 6 s GOP at 40–46.
    fn model() -> Cut {
        let mut cut = Cut::new(Path::new("/tmp/clip.mp4"), 60.0);
        cut.set_keys((0..30).map(|i| i as f64 * 2.0).filter(|k| !(*k > 40.0 && *k < 46.0)).collect());
        cut
    }

    #[test]
    fn in_snaps_back_out_snaps_forward_and_the_warning_names_the_cost() {
        let mut cut = model();
        cut.set_in(11.3);
        cut.set_out(41.5);
        assert_eq!(cut.selection(), Some((10.0, 46.0)));
        let warning = cut.warning().expect("out is deep inside the long GOP");
        assert!(warning.contains("6.0 s GOP") && warning.contains("4.50 s"), "{warning}");
        // On a keyframe already: nothing moves, nothing to warn about.
        cut.set_in(10.0);
        cut.set_out(20.0);
        assert_eq!(cut.selection(), Some((10.0, 20.0)));
        assert!(cut.warning().is_none());
        // Frame snap leaves the request alone.
        cut.snap = Snap::Frame;
        cut.set_in(11.3);
        cut.set_out(19.1);
        assert_eq!(cut.selection(), Some((11.3, 19.1)));
    }

    #[test]
    fn cuts_merge_segments_alternate_and_undo_walks_back() {
        let mut cut = model();
        assert!(!cut.cut_selection(), "no selection, nothing to cut");
        cut.set_in(10.0);
        cut.set_out(14.0);
        assert!(cut.cut_selection());
        assert!(cut.in_req.is_none() && cut.out_req.is_none());
        cut.set_in(30.0);
        cut.set_out(32.0);
        cut.cut_selection();
        let names: Vec<(bool, usize)> = cut.segments().iter().map(|s| (s.cut, s.number)).collect();
        assert_eq!(names, [(false, 1), (true, 1), (false, 2), (true, 2), (false, 3)]);
        assert_eq!(cut.keeps(), [(0.0, 10.0), (14.0, 30.0), (32.0, 60.0)]);
        assert!((cut.kept() - 54.0).abs() < 1e-9);
        assert_eq!(cut.skip(12.0), Some(14.0));
        assert_eq!(cut.skip(14.0), None);
        // A range bridging both swallows them into one.
        cut.set_in(12.0);
        cut.set_out(31.0);
        cut.cut_selection();
        assert_eq!(cut.cuts(), [(10.0, 32.0)]);
        // The exact range again restores it.
        cut.set_in(10.0);
        cut.set_out(32.0);
        assert!(cut.selection_is_cut());
        cut.cut_selection();
        assert!(cut.cuts().is_empty());
        assert!(cut.undo());
        assert_eq!(cut.cuts(), [(10.0, 32.0)]);
        assert!(cut.undo() && cut.undo() && cut.undo());
        assert!(cut.cuts().is_empty() && !cut.undo());
    }

    #[test]
    fn dragged_edges_resize_cuts_trim_the_ends_and_never_cross() {
        let mut cut = model();
        cut.set_in(10.0);
        cut.set_out(20.0);
        cut.cut_selection();
        // The kept piece after the cut, pulled later: the cut grows, snapped.
        cut.begin_trim();
        assert_eq!(cut.move_edge(20.0, 24.9), 24.0);
        assert_eq!(cut.cuts(), [(10.0, 24.0)]);
        // The piece before it, pulled earlier — and it cannot pass the start.
        assert_eq!(cut.move_edge(10.0, -3.0), 0.0);
        assert_eq!(cut.cuts(), [(0.0, 24.0)]);
        assert!(cut.undo());
        assert_eq!(cut.cuts(), [(10.0, 20.0)], "one undo step for the whole drag");
        // An edge pushed across its cut closes the cut.
        cut.begin_trim();
        assert_eq!(cut.move_edge(10.0, 30.0), 20.0);
        assert!(cut.cuts().is_empty());
        cut.undo();
        // The clip's own end trims a new cut in; dragging back removes it.
        cut.begin_trim();
        let at = cut.move_edge(60.0, 52.7);
        assert_eq!(cut.cuts(), [(10.0, 20.0), (52.0, 60.0)]);
        assert_eq!(cut.move_edge(at, 60.0), 60.0);
        assert_eq!(cut.cuts(), [(10.0, 20.0)]);
        // The start edge cannot cross the cut that is already there.
        assert_eq!(cut.move_edge(0.0, 15.0), 10.0);
        assert_eq!(cut.cuts(), [(0.0, 20.0)]);
    }

    #[test]
    fn splits_add_boundaries_only_in_kept_material() {
        let mut cut = model();
        assert!(cut.split(21.0), "snaps to the keyframe at 20");
        assert_eq!(cut.splits(), [20.0]);
        assert!(!cut.split(20.5), "same boundary");
        cut.set_in(30.0);
        cut.set_out(34.0);
        cut.cut_selection();
        assert!(!cut.split(32.0), "inside a cut");
        assert_eq!(cut.segments().len(), 4);
        assert_eq!(cut.keeps(), [(0.0, 30.0), (34.0, 60.0)], "a split does not break the export's ranges");
    }

    #[test]
    fn the_view_fits_zooms_about_the_anchor_and_pages_to_the_playhead() {
        let mut cut = model();
        cut.fit_view(600.0);
        assert_eq!(cut.pps, 18.0);
        cut.pan(-5.0, 600.0);
        assert_eq!(cut.t0, 0.0);
        cut.pan(10.0, 600.0);
        let under = cut.t0 + 300.0 / cut.pps;
        cut.zoom(2.0, 300.0, 600.0);
        assert!((cut.t0 + 300.0 / cut.pps - under).abs() < 1e-6);
        cut.zoom(0.001, 300.0, 600.0);
        assert!((cut.pps - 10.0).abs() < 1e-9, "never wider than the whole clip");
        cut.zoom(4.0, 0.0, 600.0);
        cut.reveal(50.0, 600.0);
        assert!(cut.t0 <= 50.0 && 50.0 <= cut.t0 + cut.span(600.0));
    }

    #[test]
    fn srt_cues_parse_and_shed_their_markup() {
        let cues = parse_srt("1\n00:00:01,500 --> 00:00:04,000\n<i>So the first</i>\nthing\n\n2\n00:01:00,000 --> 00:01:02,250\nrebuild\n");
        assert_eq!(cues.len(), 2);
        assert_eq!((cues[0].start, cues[0].end), (1.5, 4.0));
        assert_eq!(cues[0].text, "So the first thing");
        assert_eq!((cues[1].start, cues[1].end), (60.0, 62.25));
    }

    fn probe_duration(path: &Path) -> f64 {
        let out = Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
            .arg(path)
            .output()
            .expect("ffprobe");
        String::from_utf8_lossy(&out.stdout).trim().parse().expect("duration")
    }

    /// The real pipeline on a generated clip: the scan finds the keyframes
    /// the encoder was told to place, the stream copy removes exactly the
    /// snapped range, and what it wrote still starts on a keyframe.
    #[test]
    fn scan_finds_keyframes_and_the_export_drops_the_cut_range() {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join("abner_cut_test");
        let _ = std::fs::create_dir_all(&dir);
        let clip = dir.join("gop.mp4");
        if !clip.exists() {
            let ok = Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=duration=6:size=320x180:rate=30"])
                .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=6"])
                .args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p"])
                .args(["-g", "30", "-keyint_min", "30", "-sc_threshold", "0", "-bf", "0", "-c:a", "aac"])
                .arg(&clip)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            assert!(ok, "failed to generate test clip");
        }
        let found = scan(&clip).expect("scan");
        assert_eq!(found.keys.len(), 6, "{:?}", found.keys);
        let mut cut = Cut::new(&clip, 6.0);
        cut.set_keys(found.keys);
        let (tx, rx) = channel();
        read_wave(&clip, &AtomicBool::new(false), &tx);
        drop(tx);
        let wave: Vec<u8> = rx.iter().flatten().collect();
        assert!((wave.len() as f64 - 6.0 * WAVE_HZ).abs() <= 4.0, "{} peaks", wave.len());
        assert!(wave.iter().any(|p| *p > 8), "a sine is not silence");
        cut.set_in(2.4);
        cut.set_out(3.6);
        assert_eq!(cut.selection(), Some((2.0, 4.0)));
        cut.cut_selection();
        for (snap, name) in [(Snap::Keyframe, "copy.mp4"), (Snap::Frame, "encode.mp4")] {
            let dest = dir.join(name);
            let _ = std::fs::remove_file(&dest);
            export(&clip, &dest, &cut.keeps(), snap, &cut.chapters).expect("export");
            let d = probe_duration(&dest);
            assert!((d - 4.0).abs() < 0.15, "{name}: {d} s, wanted 4");
            let leftovers = std::fs::read_dir(&dir).unwrap().filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().contains(".tmp")).count();
            assert_eq!(leftovers, 0, "temporaries are cleaned up");
        }
    }

    #[test]
    fn stream_facts_read_from_ffprobe_json() {
        let v: serde_json::Value = serde_json::from_str(r#"{
          "streams": [
            {"codec_type":"video","codec_name":"h264","profile":"High","level":51,"width":3840,"height":2160,
             "avg_frame_rate":"24000/1001","pix_fmt":"yuv420p","color_space":"bt709","color_range":"tv","bit_rate":"48200000"},
            {"codec_type":"audio","codec_name":"aac","profile":"LC","channels":2,"channel_layout":"stereo","sample_rate":"48000","bit_rate":"320000"},
            {"codec_type":"subtitle","codec_name":"subrip"}
          ], "format": {"bit_rate":"50000000"}}"#).unwrap();
        let f = parse_facts(&v);
        assert_eq!(f.video_title, "H.264 · High · 5.1");
        assert_eq!(f.video[0], ("Size", "3840 × 2160".to_string()));
        assert_eq!(f.video[1], ("Rate", "23.976 fps".to_string()));
        assert_eq!(f.video[2], ("Depth", "8-bit · 4:2:0".to_string()));
        assert_eq!(f.video[3], ("Colour", "BT709 · tv".to_string()));
        assert_eq!(f.video[4], ("Bitrate", "48.2 Mb/s".to_string()));
        assert_eq!(f.audio_title, "AAC LC");
        assert_eq!(f.audio[1], ("Rate", "48 kHz · 320 kb/s".to_string()));
        assert_eq!(f.subs_title, "SRT");
        assert_eq!(f.chips, vec!["H.264", "2160p", "23.976", "AAC 2ch", "SRT"]);
        // A file with no audio or subtitles leaves those blank.
        let v: serde_json::Value = serde_json::from_str(r#"{"streams":[{"codec_type":"video","codec_name":"hevc","width":1920,"height":1080,"avg_frame_rate":"25/1","pix_fmt":"yuv420p10le"}]}"#).unwrap();
        let f = parse_facts(&v);
        assert_eq!(f.chips, vec!["H.265", "1080p", "25"]);
        assert!(f.audio_title.is_empty() && f.subs_title.is_empty());
        assert_eq!(f.video[2].1, "10-bit · 4:2:0");
    }

    #[test]
    fn gop_stats_name_the_median_and_the_longest() {
        let mut c = Cut::new(Path::new("x.mp4"), 30.0);
        assert!(c.gop_stats().is_none());
        c.set_keys(vec![0.0, 2.0, 4.0, 10.0, 12.0]);
        assert_eq!(c.gop_stats(), Some((5, 2.0, 6.0)));
    }

    #[test]
    fn added_chapters_sort_refuse_near_duplicates_and_move_with_the_cuts() {
        let mut c = Cut::new(Path::new("x.mp4"), 60.0);
        assert_eq!(c.add_chapter(30.0), Some(1));
        assert_eq!(c.add_chapter(10.0), Some(1), "inserted in time order");
        assert_eq!(c.add_chapter(10.3), None, "within half a second of one that exists");
        assert!(c.chapters_edited);
        assert_eq!(c.chapters.iter().map(|c| c.title.as_str()).collect::<Vec<_>>(), ["Chapter 2", "Chapter 1"]);
        // Cut 20–40: the chapter at 30 is inside it and goes; 10 stays put.
        let keeps = [(0.0, 20.0), (40.0, 60.0)];
        c.add_chapter(50.0);
        let out = output_chapters(&c.chapters, &keeps);
        assert_eq!(out.iter().map(|o| (o.0, o.1)).collect::<Vec<_>>(), [(10.0, 30.0), (30.0, 40.0)]);
        // 50 s in the source is 30 s out (20 s were cut before it).
        let meta = ffmetadata(&out);
        assert!(meta.starts_with(";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=10000\nEND=30000\ntitle=Chapter 2"));
    }
}
