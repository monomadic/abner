//! Built-in mask proposals from stationary overlays over changing nearby frames.
//! Owns a background decoder; never seeks the visible player or writes media.
mod blend;
mod evidence;
mod planes;
mod reservoir;
mod search;
mod static_overlay;
mod stroke_lift;
use crate::probe::VideoInfo;
pub(crate) use evidence::{DEFAULT_SENSITIVITY, Evidence, MAX_SENSITIVITY};
use filmcraft_frame::{PixelData, VideoFrame};
use rayon::prelude::*;
use reservoir::{Reservoir, Thumbnail, thumbnail};
use search::{DETECTOR_BUDGET, DETECTOR_RADIUS, Region, Search};
use std::{
    borrow::Borrow,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver},
    },
    time::Instant,
};
#[cfg(test)]
use std::time::Duration;

#[derive(Clone)]
struct Frame {
    width: usize,
    height: usize,
    samples: Samples,
}

#[derive(Clone)]
enum Samples {
    Rgb(Vec<u8>),
    Native(planes::Planes),
}
impl Frame {
    fn new(width: usize, height: usize, rgb: Vec<u8>) -> Result<Self, String> {
        if width.checked_mul(height).and_then(|v| v.checked_mul(3)) != Some(rgb.len())
            || width == 0
            || height == 0
        {
            return Err("Invalid sampled frame dimensions".into());
        }
        Ok(Self { width, height, samples: Samples::Rgb(rgb) })
    }
    fn rgb(&self) -> Option<&[u8]> {
        match &self.samples {
            Samples::Rgb(rgb) => Some(rgb),
            Samples::Native(_) => None,
        }
    }

    fn native(&self) -> Option<&planes::Planes> {
        match &self.samples {
            Samples::Native(planes) => Some(planes),
            Samples::Rgb(_) => None,
        }
    }

    fn luma_code(&self, x: usize, y: usize) -> u16 {
        self.native().map_or_else(
            || (self.luma(x, y) * 65535.).round() as u16,
            |planes| planes.luma[y * self.width + x],
        )
    }

    fn luma(&self, x: usize, y: usize) -> f32 {
        if let Some(planes) = self.native() {
            return planes::normalize(planes.luma[y * self.width + x]);
        }
        let [r, g, b] = self.rgb_pixel(x, y);
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    fn rgb_pixel(&self, x: usize, y: usize) -> [f32; 3] {
        let p = (y * self.width + x) * 3;
        let rgb = self.rgb().expect("RGB observation");
        std::array::from_fn(|c| rgb[p + c] as f32 / 255.)
    }

    fn edge_pixel(&self, x: usize, y: usize, native: bool) -> [f32; 3] {
        if let Some(planes) = self.native() {
            return planes.pixel(x, y, self.width);
        }
        let rgb = self.rgb_pixel(x, y);
        if !native {
            return rgb;
        }
        let y = self.luma(x, y);
        [y, 0.5 + (rgb[2] - y) / 1.8556, 0.5 + (rgb[0] - y) / 1.5748]
    }
}

fn native_luma(frames: &[Frame]) -> bool {
    frames.iter().any(|frame| frame.native().is_some())
}

fn has_deep_planes(picture: &VideoFrame) -> bool {
    matches!(&picture.data, PixelData::Yuv16 { bits, .. } if (9..=16).contains(bits))
}

fn variation_floor(frames: &[Frame], byte_floor: f32) -> f32 {
    frames.iter().map(|frame| frame.native().map_or(1., |p| p.noise_scale))
        .fold(0., f32::max) * byte_floor
}

type Check<'a> = &'a (dyn Fn() -> Result<(), String> + Sync);

/// Robust central temporal range, averaged over RGB. Convert only the chosen
/// quantiles to f32, keeping the same arithmetic as the detector's pixel values.
fn temporal_rows<T: Send>(
    frames: &[Frame],
    [x, y, w]: [usize; 3],
    rows: impl IndexedParallelIterator<Item = T>,
    write: impl Fn(&mut T, usize, [f32; 3], f32) + Sync,
    check: Check<'_>,
) -> Result<(), String> {
    let n = frames.len();
    let (lo, mid, hi) = (n / 8, n / 2, n * 7 / 8);
    if native_luma(frames) {
        return rows.enumerate().try_for_each_init(|| vec![0u16; n], |values, (row, mut output)| {
            check()?;
            for col in 0..w {
                for (value, frame) in values.iter_mut().zip(frames) {
                    *value = frame.luma_code(x + col, y + row);
                }
                // Partition the ranks independently within the two halves.
                let (lower, median, upper) = values.select_nth_unstable(mid);
                let a = *lower.select_nth_unstable(lo).1;
                let b = *upper.select_nth_unstable(hi - mid - 1).1;
                write(&mut output, col, [planes::normalize(*median); 3], planes::normalize(b - a));
            }
            Ok(())
        });
    }

    rows
        .enumerate()
        .try_for_each_init(
            || (Vec::with_capacity(n), vec![0u8; n]),
            |(rows, values), (row, mut output)| {
                check()?;
                rows.clear();
                rows.extend(frames.iter().map(|frame| {
                    let start = ((y + row) * frame.width + x) * 3;
                    &frame.rgb().expect("RGB observations")[start..start + w * 3]
                }));
                for col in 0..w {
                    let mut median = [0.; 3];
                    let mut range = 0.;
                    for (channel, level) in median.iter_mut().enumerate() {
                        let at = col * 3 + channel;
                        let [lower, middle, upper] = if n > 256 {
                            let mut counts = [0usize; 256];
                            for row in rows.iter() {
                                counts[row[at] as usize] += 1;
                            }
                            let mut found = [0; 3];
                            let mut total = 0;
                            let mut rank = 0;
                            let ranks = [lo, mid, hi];
                            for (byte, count) in counts.into_iter().enumerate() {
                                total += count;
                                while rank < ranks.len() && ranks[rank] < total {
                                    found[rank] = byte as u8;
                                    rank += 1;
                                }
                                if rank == ranks.len() {
                                    break;
                                }
                            }
                            found
                        } else {
                            for (value, row) in values.iter_mut().zip(rows.iter()) {
                                *value = row[at];
                            }
                            values.sort_unstable();
                            [values[lo], values[mid], values[hi]]
                        };
                        *level = middle as f32 / 255.;
                        range += (upper as f32 / 255. - lower as f32 / 255.) / 3.;
                    }
                    write(&mut output, col, median, range);
                }
                Ok(())
            },
        )
}

/// Detection uses source time, independently of the visible playhead.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Range { pub start:f64, pub end:f64 }
impl Range {
    pub fn near(at:f64,duration:f64)->Self { Self{start:(at-30.).max(0.),end:(at+30.).min(duration)} }
    #[cfg(test)]
    pub fn samples(self,duration:f64,fps:f64)->Result<Vec<f64>,String> {
        self.samples_count(duration,fps,Some(64))
    }
    /// None means every frame in the selected range, not an unlimited sample cap.
    pub fn samples_count(
        self,
        duration: f64,
        fps: f64,
        count: Option<usize>,
    ) -> Result<Vec<f64>, String> {
        let plan = self.sample_plan(duration, fps, count)?;
        Ok((0..plan.count).map(|i| plan.at(i)).collect())
    }

    fn sample_plan(self, duration: f64, fps: f64, count: Option<usize>) -> Result<SamplePlan, String> {
        if !duration.is_finite()
            || !fps.is_finite()
            || duration <= 0.
            || fps <= 0.
            || !self.start.is_finite()
            || !self.end.is_finite()
            || self.start < 0.
            || self.end > duration
            || self.start >= self.end
        {
            return Err("Select a valid detection range inside the clip".into());
        }
        let first = (self.start * fps - 1e-7).ceil().max(0.) as usize;
        let last = ((self.end * fps + 1e-7).floor() as usize)
            .min(((duration * fps - 1e-7).ceil() as usize).saturating_sub(1));
        let available = last.saturating_sub(first).saturating_add(1);
        let count = count.unwrap_or(available).min(available);
        if count < 6 || last < first {
            return Err("Select at least six distinct frames for detection".into());
        }
        if count > 10_000_000 {
            return Err("Select a shorter detection range".into());
        }
        Ok(SamplePlan { first, last, count, fps })
    }
    pub fn set_edge(&mut self,end:bool,at:f64,duration:f64,fps:f64) {
        let minimum=(6./fps.max(1.)).min(duration);
        let t=(at*fps.max(1.)).round()/fps.max(1.);
        if end {self.end=t.clamp((self.start+minimum).min(duration),duration);}else{self.start=t.clamp(0.,(self.end-minimum).max(0.));}
    }
}

/// The detection frame grid. Every frame advances it without allocating a time per frame.
struct SamplePlan {
    first: usize,
    last: usize,
    count: usize,
    fps: f64,
}

impl SamplePlan {
    fn at(&self, i: usize) -> f64 {
        let offset = (i as u128 * (self.last - self.first) as u128 / (self.count - 1) as u128) as usize;
        (self.first + offset) as f64 / self.fps
    }
}

pub(crate) struct Proposal {
    pub pixels: Vec<u8>,
    pub samples: usize,
    pub evidence: Option<Evidence>,
    pub barely_changes: bool,
}

const STALL_SECONDS: usize = 180;

struct Progress {
    count: AtomicUsize,
    started: Instant,
    last_activity: AtomicUsize,
}

impl Progress {
    fn new() -> Self {
        Self {
            count: AtomicUsize::new(0),
            started: Instant::now(),
            last_activity: AtomicUsize::new(0),
        }
    }

    fn checkpoint(&self, cancel: &AtomicBool) -> Result<(), String> {
        if cancel.load(Ordering::Relaxed) {
            return Err("Watermark detection stopped".into());
        }
        let now = self.started.elapsed().as_secs() as usize;
        let last = self.last_activity.load(Ordering::Relaxed);
        if now.saturating_sub(last) > STALL_SECONDS {
            return Err("Watermark detection stalled".into());
        }
        if now > last {
            self.last_activity.fetch_max(now, Ordering::Relaxed);
        }
        Ok(())
    }

    fn expired(&self) -> bool {
        (self.started.elapsed().as_secs() as usize)
            .saturating_sub(self.last_activity.load(Ordering::Relaxed)) > STALL_SECONDS
    }
}

pub(crate) struct Task {
    pub source: usize,
    pub mask_id: u64,
    pub revision: u64,
    pub region: Option<Region>,
    pub receiver: Receiver<Result<Proposal, String>>,
    progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
    total: usize,
}
impl Drop for Task {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}
impl Task {
    #[cfg(test)]
    pub fn test_pending(
        source: usize,
        mask_id: u64,
        revision: u64,
    ) -> (
        Self,
        std::sync::mpsc::Sender<Result<Proposal, String>>,
        Arc<AtomicBool>,
    ) {
        let (sender, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        (
            Self {
                source,
                mask_id,
                revision,
                region: None,
                receiver,
                progress: Arc::new(Progress::new()),
                cancel: cancel.clone(),
                total: 64,
            },
            sender,
            cancel,
        )
    }

    /// Frames read so far, of how many.
    pub fn progress_counts(&self) -> (usize, usize) {
        (self.progress.count.load(Ordering::Relaxed).min(self.total), self.total)
    }
    pub fn status(&self) -> String {
        let n = self.progress.count.load(Ordering::Relaxed);
        if n <= self.total {
            format!("Autodetect · reading frames {n}/{}…", self.total)
        } else if n == self.total + 1 {
            "Autodetect · analysing static pixels…".into()
        } else {
            "Autodetect · refining mask…".into()
        }
    }
    pub fn expired(&self) -> bool {
        self.progress.expired()
    }
    // Keep the explicit geometry/pipeline inputs at this boundary.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        info: VideoInfo,
        source: usize,
        mask_id: u64,
        revision: u64,
        region: Option<[usize; 4]>,
        range: Range,
        drawn: bool,
        samples: Option<usize>,
        review: bool,
    ) -> Result<Self, String> {
        let region = detection_region(region, info.width as usize, info.height as usize)?;
        let plan = range.sample_plan(info.duration, info.fps, samples)?;
        let total = plan.count;
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let progress = Arc::new(Progress::new());
        let count = progress.clone();
        let (sender, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("watermark detection".into())
            .spawn(move || {
                let result = filmcraft_media::cancel::with_cancel(&flag, || {
                    filmcraft_media::cancel::with_background(true, || {
                        analyze(
                            info.path,
                            info.width,
                            info.height,
                            plan,
                            samples.map(|_| range),
                            &flag,
                            &count,
                            Some(region),
                            drawn,
                            review,
                        )
                    })
                });
                let _ = sender.send(result);
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            source,
            mask_id,
            revision,
            region: Some(region),
            receiver,
            progress,
            cancel,
            total,
        })
    }
}

#[cfg(test)]
fn sample_times(at:f64,duration:f64,fps:f64)->Result<Vec<f64>,String> {
    Range::near(at.clamp(0.,duration.max(0.)),duration).samples_count(duration,fps,Some(16))
}

fn detection_region(region: Option<[usize;4]>, w:usize, h:usize) -> Result<[usize;4],String> {
    let [x,y,rw,rh] = region.unwrap_or([0,0,w,h]);
    if x>=w || y>=h || rw==0 || rh==0 {return Err("The processing region is empty or outside the image".into());}
    Ok([x,y,rw.min(w-x),rh.min(h-y)])
}
fn mark_region(pixels: &mut [u8], width: usize, [x, y, w, h]: Region, selected: &[u8]) {
    for (row, flags) in selected.chunks_exact(w).take(h).enumerate() {
        let start = (y + row) * width + x;
        for (pixel, &mark) in pixels[start..start + w].iter_mut().zip(flags) {
            if mark != 0 {
                *pixel = 255;
            }
        }
    }
}

fn snap_samples(times: &[f64], keyframes: &[f64]) -> Vec<Option<f64>> {
    let mut snapped = vec![None; times.len()];
    if keyframes.is_empty() {
        return snapped;
    }
    // Several targets can share their nearest keyframe. Use it for the closest
    // target and read the other targets at their original times.
    let mut previous: Option<(usize, usize)> = None;
    for (i, &t) in times.iter().enumerate() {
        let after = keyframes.partition_point(|&key| key <= t);
        let before = after.saturating_sub(1);
        let nearest = match keyframes.get(after) {
            Some(&key) if key - t < t - keyframes[before] => after,
            _ => before,
        };
        let key = keyframes[nearest];
        if let Some((_, slot)) = previous.filter(|&(last, _)| last == nearest) {
            if (t - key).abs() >= (times[slot] - key).abs() {
                continue;
            }
            snapped[slot] = None;
        }
        snapped[i] = Some(key);
        previous = Some((nearest, i));
    }
    snapped
}

// Keep the explicit geometry/pipeline inputs at this boundary.
#[allow(clippy::too_many_arguments)]
fn analyze(
    path: PathBuf,
    w: u32,
    h: u32,
    plan: SamplePlan,
    sparse_range: Option<Range>,
    cancel: &AtomicBool,
    progress: &Progress,
    region: Option<Region>,
    drawn: bool,
    review: bool,
) -> Result<Proposal, String> {
    let region = detection_region(region, w as usize, h as usize)?;
    let total = plan.count;
    #[cfg(test)]
    let started = Instant::now();
    let check = || progress.checkpoint(cancel);
    let progress = &progress.count;
    check()?;
    let snapped = sparse_range.and_then(|range| {
        let table = crate::fc::SyncTable::for_frame_requests(&path)?;
        let times: Vec<_> = (0..total).map(|i| plan.at(i)).collect();
        Some(snap_samples(&times, table.in_range(range.start, range.end)))
    });
    let mut sampling = Sampling {
        path: &path,
        size: [w, h],
        plan,
        snapped,
        primed: None,
        check: &check,
        #[cfg(test)]
        read_elapsed: started.elapsed(),
    };
    let size = [w as usize, h as usize];
    let mut barely_changes = false;
    let mut retained = if sparse_range.is_none() {
        let mut retained = Reservoir::new(region, Search::context(size, region), DETECTOR_BUDGET / 2);
        sampling.scan(total, |i, picture| {
            retained.consider(i, picture);
            progress.store(i + 1, Ordering::Relaxed);
            Ok(())
        })?;
        barely_changes = retained.barely_changes();
        if retained.len() < 6 {
            return insufficient_samples(size, retained.len(), barely_changes);
        }
        retained.finish();
        progress.store(total + 1, Ordering::Relaxed);
        Some(retained)
    } else {
        None
    };
    let count = retained.as_ref().map_or(total, Reservoir::len);
    let budget = DETECTOR_BUDGET - retained.as_ref().map_or(0, Reservoir::bytes);
    // Price the captured planes from the decoded format. A sparse scan reuses
    // its first reader and picture after planning.
    let native = if let Some(retained) = &retained {
        retained.pictures(count).next().is_some_and(has_deep_planes)
    } else {
        sampling.prime()?
    };
    // A drawn box in a moving scene keeps the RGB stroke detector's existing
    // behavior. Save the native planes as a second source of evidence in quiet boxes.
    let drawn_box = drawn && region[2] * region[3] <= size[0] * size[1] / 4;
    let mut search = Search::with_native(size, region, count, budget, native && !drawn_box);
    let scout_count = search.scout_count(count, budget);
    if scout_count < count {
        search = Search::with_native(size, region, scout_count, budget, native && !drawn_box);
    }
    if native && drawn_box {
        search.reserve_fine_flicker(scout_count, budget);
    }
    let can_consume = search.scale == 1 && scout_count == count;
    let base = std::slice::from_mut(&mut search);
    if can_consume && let Some(retained) = retained.take() {
        // Native analysis needs these pictures only once. Release each as it is converted.
        sampling.read_retained(base, retained.origin(), retained.into_pictures())?;
    } else if let Some(retained) = &retained {
        sampling.read_retained(base, retained.origin(), retained.pictures(scout_count))?;
    } else {
        let mut thumbnails = Vec::new();
        sampling.read(base, scout_count, Some((progress, region, &mut thumbnails)))?;
        barely_changes = reservoir::barely_changes(&thumbnails);
    }
    let mut fine_flicker = search.fine_flicker.take().unwrap_or_default();
    let local_region = search.local_region();
    if search.frames.len() < 6 && stroke_lift::quiet(&fine_flicker, local_region) {
        search.frames = std::mem::take(&mut fine_flicker);
    }
    let mut samples = search.frames.len();
    if samples < 6 {
        return insufficient_samples(size, samples, barely_changes);
    }
    progress.store(total + 1, Ordering::Relaxed);
    let mut evidence = review.then(Evidence::default);
    let mut maps = detect_search(&search, &check)?;
    let mut pixels = vec![0; size[0] * size[1]];
    if search.scale == 1 {
        let blend_veto = std::mem::take(&mut maps.blend_veto);
        let mut selected = native_selection(maps, region, &mut evidence, &check)?;
        // A tightly drawn box permits evidence over stationary scenery.
        if drawn_box && !selected.contains(&255) {
            let mut scores = stroke_lift::scores_checked(&search.frames, local_region, &blend_veto, &check)?;
            if !scores.iter().any(|&score| evidence::selected_by_default(score))
                && stroke_lift::quiet(&fine_flicker, local_region)
            {
                scores = stroke_lift::scores_checked(&fine_flicker, local_region, &blend_veto, &check)?;
                samples = samples.max(fine_flicker.len());
            }
            selected = scores.iter().map(|&score| u8::from(evidence::selected_by_default(score)) * 255).collect();
            if let Some(evidence) = &mut evidence && selected.contains(&255) {
                evidence.set_strokes(region, scores);
            }
        }
        mark_region(&mut pixels, size[0], region, &selected);
    } else {
        let level = if review { MAX_SENSITIVITY } else { DEFAULT_SENSITIVITY };
        let mut selected = maps.selection(search.local_region()[2], level, &check)?;
        for (pixel, &seed) in selected.iter_mut().zip(&maps.refine_seeds) {
            if seed { *pixel = 255; }
        }
        drop(maps);
        let batches = search.refinements(&selected, size, count, budget, &check)?;
        #[cfg(test)]
        eprintln!(
            "Autodetect coarse scale: {}x, {} samples, {} refinement windows",
            search.scale,
            samples,
            batches.iter().map(Vec::len).sum::<usize>(),
        );
        drop(search);
        if !batches.is_empty() {
            progress.store(total + 2, Ordering::Relaxed);
        }
        for mut batch in batches {
            if let Some(retained) = &retained {
                sampling.read_retained(&mut batch, retained.origin(), retained.pictures(count))?;
            } else {
                sampling.read(&mut batch, count, None)?;
            }
            for search in batch {
                if search.frames.len() >= 6 {
                    samples = samples.max(search.frames.len());
                    let maps = detect_search(&search, &check)?;
                    let selected = native_selection(maps, search.region, &mut evidence, &check)?;
                    mark_region(&mut pixels, size[0], search.region, &selected);
                }
            }
        }
    }
    check()?;
    #[cfg(test)]
    eprintln!(
        "Autodetect batch: read {:.6}s, analyse {:.6}s",
        sampling.read_elapsed.as_secs_f64(),
        started.elapsed().saturating_sub(sampling.read_elapsed).as_secs_f64(),
    );
    Ok(Proposal { pixels, samples, evidence, barely_changes })
}

fn insufficient_samples(
    [w, h]: [usize; 2], samples: usize, barely_changes: bool,
) -> Result<Proposal, String> {
    if barely_changes {
        Ok(Proposal { pixels: vec![0; w * h], samples, evidence: None, barely_changes })
    } else {
        Err("Too few distinct decoded frames to detect a watermark".into())
    }
}

fn detect_search(search: &Search, check: Check<'_>) -> Result<static_overlay::Maps, String> {
    static_overlay::detect_checked(&search.frames, search.local_region(), DETECTOR_RADIUS, search.scale, check)
}

fn native_selection(
    maps: static_overlay::Maps,
    region: Region,
    evidence: &mut Option<Evidence>,
    check: Check<'_>,
) -> Result<Vec<u8>, String> {
    let selected = maps.selection(region[2], DEFAULT_SENSITIVITY, check)?;
    if let Some(evidence) = evidence {
        evidence.add_window(region, maps.opacity, maps.confidence, maps.min_component);
    }
    Ok(selected)
}

struct Sampling<'a> {
    path: &'a Path,
    size: [u32; 2],
    plan: SamplePlan,
    snapped: Option<Vec<Option<f64>>>,
    primed: Option<(filmcraft_media::SharedSource, Arc<VideoFrame>)>,
    check: Check<'a>,
    #[cfg(test)]
    read_elapsed: Duration,
}

impl Sampling<'_> {
    fn read(
        &mut self,
        searches: &mut [Search],
        count: usize,
        mut report: Option<(&AtomicUsize, Region, &mut Vec<Thumbnail>)>,
    ) -> Result<(), String> {
        let mut rgba = Vec::new();
        self.scan(count, |i, picture| {
            Self::prepare(&picture, &mut rgba, searches, [0, 0])?;
            if let Some((progress, region, thumbnails)) = &mut report {
                if i % count.div_ceil(64) == 0 {
                    thumbnails.push(thumbnail(&picture, *region));
                }
                progress.store(i + 1, Ordering::Relaxed);
            }
            Ok(())
        })
    }

    fn read_retained(
        &mut self,
        searches: &mut [Search],
        origin: [usize; 2],
        pictures: impl Iterator<Item = impl Borrow<VideoFrame>>,
    ) -> Result<(), String> {
        #[cfg(test)]
        let started = Instant::now();
        let mut rgba = Vec::new();
        for picture in pictures {
            (self.check)()?;
            Self::prepare(picture.borrow(), &mut rgba, searches, origin)?;
        }
        #[cfg(test)]
        {
            self.read_elapsed += started.elapsed();
        }
        Ok(())
    }

    fn prepare(
        picture: &VideoFrame,
        rgba: &mut Vec<u8>,
        searches: &mut [Search],
        origin: [usize; 2],
    ) -> Result<(), String> {
        if searches.first().is_some_and(|search| search.native)
            && has_deep_planes(picture)
        {
            for search in searches {
                search.add_native(picture, origin)?;
            }
            return Ok(());
        }
        let cropped;
        let (rgb_picture, rgb_origin) = if origin == [0, 0] {
            let crop = Search::conversion_crop(searches, picture);
            if crop.w * crop.h <= picture.width as usize * picture.height as usize / 2 {
                cropped = picture.cropped(crop);
                (&cropped, [crop.x, crop.y])
            } else {
                (picture, origin)
            }
        } else {
            (picture, origin)
        };
        let (w, h) = (rgb_picture.width, rgb_picture.height);
        crate::fc::fit_rgba_into(rgb_picture, crate::fc::Fit::Exact { w, h }, rgba);
        for search in searches {
            search.add(rgba, w as usize, rgb_origin)?;
            if search.fine_flicker.is_some() && has_deep_planes(picture) {
                search.add_native(picture, origin)?;
            }
        }
        Ok(())
    }

    fn prime(&mut self) -> Result<bool, String> {
        #[cfg(test)]
        let started = Instant::now();
        (self.check)()?;
        let source = crate::fc::open(self.path)?;
        let frame_s = crate::fc::timing(&source).map_or(0., |v| v.0);
        let picture = self.read_picture(&source, 0, frame_s)?;
        let native = has_deep_planes(&picture);
        self.primed = Some((source, picture));
        #[cfg(test)]
        {
            self.read_elapsed += started.elapsed();
        }
        Ok(native)
    }

    fn read_picture(
        &self, source: &filmcraft_media::SharedSource, i: usize, frame_s: f64,
    ) -> Result<Arc<VideoFrame>, String> {
        let time = self.snapped.as_ref().and_then(|times| times[i])
            .unwrap_or_else(|| self.plan.at(i) + frame_s * 0.25);
        let picture = source.video_frame(filmcraft_media::FrameRequest::full(
            filmcraft_time::Tick::from_seconds_f64(time),
        )).map_err(|e| format!("Could not read a nearby frame: {e}"))?;
        if [picture.width, picture.height] != self.size {
            return Err("Sampled frame geometry does not match the mask".into());
        }
        Ok(picture)
    }

    fn scan(
        &mut self,
        count: usize,
        mut visit: impl FnMut(usize, Arc<VideoFrame>) -> Result<(), String>,
    ) -> Result<(), String> {
        #[cfg(test)]
        let started = Instant::now();
        (self.check)()?;
        let (source, mut first) = match self.primed.take() {
            Some((source, picture)) => (source, Some(picture)),
            None => (crate::fc::open(self.path)?, None),
        };
        let frame_s = crate::fc::timing(&source).map_or(0., |v| v.0);
        for sample in 0..count {
            (self.check)()?;
            let i = sample * (self.plan.count - 1) / (count - 1);
            let picture = match first.take() {
                Some(picture) => picture,
                None => self.read_picture(&source, i, frame_s)?,
            };
            visit(sample, picture)?;
        }
        drop(source);
        #[cfg(test)]
        {
            self.read_elapsed += started.elapsed();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchdog_expires_on_inactivity_despite_a_large_frame_count() {
        let progress = Progress {
            started: Instant::now() - Duration::from_secs((STALL_SECONDS * 2) as u64),
            ..Progress::new()
        };
        progress.count.store(10_000, Ordering::Relaxed);
        assert!(progress.expired());
        progress.last_activity.store(STALL_SECONDS * 2, Ordering::Relaxed);
        assert!(!progress.expired());
        progress.checkpoint(&AtomicBool::new(false)).unwrap();
    }

    #[test]
    fn deep_flicker_below_the_byte_floor_recovers_a_faint_stroke() {
        let [w, h] = [192, 128];
        let region = [32, 32, 80, 60];
        let stroke = |x, y| (55..63).contains(&x) && (45..75).contains(&y);
        let mut searches = [
            Search::with_native([w, h], region, 6, DETECTOR_BUDGET, true),
            Search::new([w, h], region, 6, DETECTOR_BUDGET),
        ];
        let chroma = Arc::new(vec![512; w * h / 4]);
        let mut rgba = Vec::new();
        for t in 0..6 {
            let luma = (0..w * h).map(|i| {
                let (y, x) = (i / w, i % w);
                let texture = (y * 131 + x * 17) % 5;
                let flicker = (t * (1 + (y * 17 + x * 3) % 7) + y + x) % 6;
                (500 + texture + if stroke(x, y) { 84 } else { flicker }) as u16
            }).collect();
            let picture = VideoFrame {
                width: w as u32,
                height: h as u32,
                data: PixelData::Yuv16 {
                    planes: [Arc::new(luma), chroma.clone(), chroma.clone()],
                    chroma: filmcraft_frame::Chroma::C420,
                    bits: 10,
                    alpha: None,
                },
                color: filmcraft_color::ColorInfo::REC709,
                par: (1, 1),
                pts: filmcraft_time::Tick::ZERO,
            };
            for search in &mut searches {
                Sampling::prepare(&picture, &mut rgba, std::slice::from_mut(search), [0, 0]).unwrap();
            }
        }
        let selected = |search: &Search| {
            stroke_lift::scores_checked(&search.frames, search.local_region(), &[], &|| Ok(())).unwrap()
                .into_iter().map(evidence::selected_by_default).collect::<Vec<_>>()
        };
        let native = selected(&searches[0]);
        assert!(!selected(&searches[1]).contains(&true));
        let hits = native.iter().enumerate().filter(|(i, keep)| {
            **keep && stroke(region[0] + i % region[2], region[1] + i / region[2])
        }).count();
        assert_eq!(hits, 8 * 30);
    }

    #[test]
    fn sparse_samples_use_each_nearest_keyframe_once_and_keep_other_targets() {
        let times = [0., 1., 2., 3., 5., 7., 9.];
        let keys = [0., 3.25, 8.];
        assert_eq!(
            snap_samples(&times, &keys),
            [Some(0.), None, None, Some(3.25), None, Some(8.), None],
        );
        assert_eq!(snap_samples(&times, &times), times.map(Some));
        assert_eq!(snap_samples(&times, &[]), [None; 7]);
        assert_eq!(snap_samples(&[2.], &[1., 3.]), [Some(1.)]);
    }
    #[test]
    fn explicit_detection_range_samples_only_its_frames_and_clamps_handles() {
        let range=Range{start:12.,end:18.};let times=range.samples(100.,30.).unwrap();
        assert_eq!(times.len(),64);assert_eq!(times[0],12.);assert_eq!(*times.last().unwrap(),18.);
        assert!(times.iter().all(|&t|(12. ..=18.).contains(&t)));
        let mut selected=range;selected.set_edge(false,90.,100.,30.);
        assert!(selected.end-selected.start>=6./30.-1e-9);
        selected.set_edge(true,0.,100.,30.);assert!(selected.end>selected.start);
        assert!(Range{start:0.,end:0.1}.samples(10.,30.).is_err());
        let full=Range{start:0.,end:2.};assert!(*full.samples(2.,30.).unwrap().last().unwrap()<2.);
    }
    #[test]
    fn selectable_counts_and_all_frames_respect_non_grid_boundaries() {
        let r=Range{start:12.01,end:18.02};
        for count in [Some(16),Some(64),Some(128),None] {
            let t=r.samples_count(100.,30.,count).unwrap();
            assert_eq!(t.len(),count.unwrap_or(180));
            assert!(t.iter().all(|&v|v>=r.start && v<=r.end));
            assert!(t.windows(2).all(|w|w[0]<w[1]));
        }
    }
    #[test]
    fn region_detection_only_marks_pixels_inside_the_processing_region() {
        let region = detection_region(Some([3,2,99,2]),8,6).unwrap();
        assert_eq!(region,[3,2,5,2]);
        let mut pixels = vec![0; 8 * 6];
        mark_region(&mut pixels, 8, region, &[255; 10]);
        for y in 0..6 { for x in 0..8 {
            assert_eq!(pixels[y*8+x],if (2..4).contains(&y) && x>=3 {255}else{0});
        }}
        assert!(detection_region(Some([8,0,1,1]),8,6).is_err());
        assert!(detection_region(Some([0,0,0,1]),8,6).is_err());
        assert_eq!(detection_region(None,8,6).unwrap(),[0,0,8,6]);
    }
    #[test]
    fn samples_are_near_playhead_distinct_and_inside_clip() {
        assert_eq!(
            sample_times(60., 200., 30.).unwrap(),
            (0..16).map(|i| 30. + i as f64 * 4.).collect::<Vec<_>>()
        );
        for at in [0., 9.99, 99.] {
            let t = sample_times(at, 10., 30.).unwrap();
            assert!(t.windows(2).all(|w| w[1] > w[0]));
            assert!(t.iter().all(|&t| (0. ..10.).contains(&t)));
        }
        assert!(sample_times(0., 0.1, 30.).is_err());
        assert!(sample_times(0., f64::NAN, 30.).is_err());
    }
    /// Explicit local diagnostic: no media path or output is built into the app.
    #[test]
    #[ignore = "set ABNER_DETECT_INPUT, ABNER_DETECT_OUTPUT and ABNER_DETECT_COUNT; optionally set ABNER_DETECT_BOX"]
    fn inspect_detection_clip() {
        let path = PathBuf::from(std::env::var("ABNER_DETECT_INPUT").unwrap());
        let info = crate::probe::probe(&path).unwrap();
        let count = match std::env::var("ABNER_DETECT_COUNT").unwrap().as_str() {
            "all" => None,
            count => Some(count.parse().unwrap()),
        };
        let region = std::env::var("ABNER_DETECT_BOX").ok().map(|box_coords| {
            let values: Vec<usize> = box_coords.split(',').map(|value| value.parse().unwrap()).collect();
            <[usize; 4]>::try_from(values).expect("ABNER_DETECT_BOX must be x,y,w,h")
        });
        let range = std::env::var("ABNER_DETECT_RANGE").map_or(
            Range { start: 0., end: info.duration },
            |range| {
                let (start, end) = range.split_once(',').expect("ABNER_DETECT_RANGE must be start,end");
                Range { start: start.parse().unwrap(), end: end.parse().unwrap() }
            },
        );
        let (w, h) = (info.width, info.height);
        let started = Instant::now();
        let before = filmcraft_codecs::gop::gop_stats();
        let review = std::env::var_os("ABNER_DETECT_REVIEW").is_some();
        let task = Task::spawn(info, 0, 0, 0, region, range, region.is_some(), count, review).unwrap();
        let proposal = loop {
            match task.receiver.recv_timeout(Duration::from_secs(1)) {
                Ok(result) => break result.unwrap(),
                Err(mpsc::RecvTimeoutError::Timeout) if !task.expired() => (),
                Err(error) => panic!("Autodetect did not return a proposal: {error}"),
            }
        };
        let decode = filmcraft_codecs::gop::gop_stats() - before;
        eprintln!(
            "Autodetect total: {:.6}s, {} samples fed to decoders, {} decoder starts",
            started.elapsed().as_secs_f64(), decode.decoded, decode.seeks,
        );
        if let Some(evidence) = &proposal.evidence {
            let size = [w as usize, h as usize];
            assert_eq!(evidence.select(size, DEFAULT_SENSITIVITY), proposal.pixels);
            for level in [0, DEFAULT_SENSITIVITY, MAX_SENSITIVITY] {
                let selection = evidence.select(size, level);
                eprintln!("Sensitivity {}%: {} selected pixels", level * 10,
                    selection.iter().filter(|&&pixel| pixel != 0).count());
            }
            assert_eq!(filmcraft_codecs::gop::gop_stats() - before, decode);
        }
        #[cfg(target_os = "macos")]
        {
            let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
            // getrusage initializes the output on success and reports bytes on macOS.
            if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } == 0 {
                let peak_bytes = unsafe { usage.assume_init() }.ru_maxrss;
                eprintln!("Autodetect peak RSS: {:.1} MiB", peak_bytes as f64 / 1048576.);
            }
        }
        let file = std::fs::File::create(std::env::var("ABNER_DETECT_OUTPUT").unwrap()).unwrap();
        let mut encoder = png::Encoder::new(file, w, h);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header().unwrap().write_image_data(&proposal.pixels).unwrap();
        eprintln!(
            "Native detector: {} samples, {} selected pixels",
            proposal.samples,
            proposal.pixels.iter().filter(|&&p| p > 0).count(),
        );
        eprintln!("Low thumbnail diversity: {}", proposal.barely_changes);
    }

    #[test]
    fn cancelled_analysis_never_opens_the_source() {
        let err = analyze(
            PathBuf::from("missing-video"),
            32,
            32,
            SamplePlan { first: 0, last: 15, count: 16, fps: 24. },
            None,
            &AtomicBool::new(true),
            &Progress::new(),
            None,
            false,
            false,
        )
        .err()
        .unwrap();
        assert_eq!(err, "Watermark detection stopped");
    }
}
