//! Bounded sampling windows for full-resolution and coarse-to-fine searches.
use super::{Check, Frame, Samples, planes::Planes};
use filmcraft_frame::{PixelData, VideoFrame};
use rayon::prelude::*;
use std::{collections::{HashMap, hash_map::Entry}, hash::{DefaultHasher, Hash, Hasher}};

pub(super) const DETECTOR_BUDGET: usize = 1536 * 1024 * 1024;
pub(super) const DETECTOR_RADIUS: usize = 32;
const HALO: usize = DETECTOR_RADIUS * 3;
const REFINE_PAD: usize = 24;

pub(super) type Region = [usize; 4];

pub(super) struct Search {
    pub region: Region,
    pub frames: Vec<Frame>,
    context: Region,
    pub scale: usize,
    pub native: bool,
    pub fine_flicker: Option<Vec<Frame>>,
    rgb_seen: HashMap<u64, Vec<usize>>,
    native_seen: HashMap<u64, Vec<usize>>,
}

impl Search {
    #[cfg(test)]
    pub fn new(size: [usize; 2], region: Region, samples: usize, budget: usize) -> Self {
        Self::with_native(size, region, samples, budget, false)
    }

    pub fn with_native(
        size: [usize; 2],
        region: Region,
        samples: usize,
        budget: usize,
        native: bool,
    ) -> Self {
        let mut search = Self {
            region,
            frames: Vec::new(),
            context: Self::context(size, region),
            scale: 1,
            native,
            fine_flicker: None,
            rgb_seen: HashMap::new(),
            native_seen: HashMap::new(),
        };
        while search.bytes(samples) > budget {
            search.scale *= 2;
        }
        search
    }

    pub fn context([w, h]: [usize; 2], region: Region) -> Region {
        let [x, y, rw, rh] = region;
        let left = x.saturating_sub(HALO);
        let top = y.saturating_sub(HALO);
        [
            left,
            top,
            (x + rw + HALO).min(w) - left,
            (y + rh + HALO).min(h) - top,
        ]
    }

    pub fn conversion_crop(searches: &[Self], picture: &VideoFrame) -> filmcraft_frame::Region {
        let [x, y, w, h] = searches[0].context;
        let (mut left, mut top, mut right, mut bottom) = (x, y, x + w, y + h);
        for search in &searches[1..] {
            let [x, y, w, h] = search.context;
            left = left.min(x);
            top = top.min(y);
            right = right.max(x + w);
            bottom = bottom.max(y + h);
        }
        let (sx, sy) = match &picture.data {
            PixelData::Yuv8 { chroma, .. } | PixelData::Yuv16 { chroma, .. } => chroma.shifts(),
            _ => (0, 0),
        };
        left = (left >> sx) << sx;
        top = (top >> sy) << sy;
        filmcraft_frame::Region { x: left, y: top, w: right - left, h: bottom - top }
    }

    fn sample_bytes(&self) -> usize {
        // Reserve for 4:4:4 even though subsampled inputs use less.
        if self.native { 6 } else { 3 }
    }

    fn bytes(&self, samples: usize) -> usize {
        let [w, h] = self.sample_size();
        w.saturating_mul(h)
            .saturating_mul(samples * self.sample_bytes() + 112)
    }

    fn sample_size(&self) -> [usize; 2] {
        [
            self.context[2].div_ceil(self.scale),
            self.context[3].div_ceil(self.scale),
        ]
    }

    /// Prefer a finer scout for large sample sets. The native refinement still
    /// gets the whole set, including the frames omitted from this pass.
    pub fn scout_count(&self, samples: usize, budget: usize) -> usize {
        if self.scale <= 2 {
            return samples;
        }
        let [_, _, w, h] = self.context;
        let half_area = w.div_ceil(2).saturating_mul(h.div_ceil(2));
        let affordable = (budget / half_area).saturating_sub(112) / self.sample_bytes();
        affordable.max(16).min(samples)
    }

    pub fn local_region(&self) -> Region {
        let [x, y, w, h] = self.region;
        let rel_x = x - self.context[0];
        let rel_y = y - self.context[1];
        let left = rel_x / self.scale;
        let top = rel_y / self.scale;
        [
            left,
            top,
            (rel_x + w).div_ceil(self.scale) - left,
            (rel_y + h).div_ceil(self.scale) - top,
        ]
    }

    pub fn reserve_fine_flicker(&mut self, samples: usize, budget: usize) {
        let [w, h] = self.sample_size();
        let extra = w.saturating_mul(h).saturating_mul(samples * 6);
        if self.scale == 1 && self.bytes(samples).saturating_add(extra) <= budget {
            self.fine_flicker = Some(Vec::new());
        }
    }

    pub fn add_native(&mut self, picture: &VideoFrame, origin: [usize; 2]) -> Result<(), String> {
        let [x, y, w, h] = self.context;
        let Some(native) =
            Planes::capture(picture, [x - origin[0], y - origin[1], w, h], self.scale)?
        else {
            return Err("Expected deep planar samples for detection".into());
        };
        let [width, height] = self.sample_size();
        let frames = self.fine_flicker.as_mut().unwrap_or(&mut self.frames);
        let hash = native.fingerprint();
        if record(&mut self.native_seen, hash, frames.len(), |i| {
            frames[i].native().is_some_and(|p| p.same_samples(&native))
        }) {
            frames.push(Frame {
                width,
                height,
                samples: Samples::Native(native),
            });
        }
        Ok(())
    }

    pub fn add(&mut self, rgba: &[u8], stride: usize, origin: [usize; 2]) -> Result<(), String> {
        let [x, y, width, height] = self.context;
        let (x, y) = (x - origin[0], y - origin[1]);
        let [w, h] = self.sample_size();
        let mut rgb = Vec::with_capacity(w * h * 3);
        if self.scale == 1 {
            for row in y..y + height {
                let start = (row * stride + x) * 4;
                for pixel in rgba[start..start + w * 4].chunks_exact(4) {
                    rgb.extend_from_slice(&pixel[..3]);
                }
            }
        } else {
            rgb.resize(w * h * 3, 0);
            let scale = self.scale;
            rgb.par_chunks_mut(w * 3)
                .enumerate()
                .for_each(|(row, output)| {
                    let top = row * scale;
                    let bottom = (top + scale).min(height);
                    for (col, pixel) in output.chunks_exact_mut(3).enumerate() {
                        let left = col * scale;
                        let right = (left + scale).min(width);
                        let mut sum = [0u64; 3];
                        for py in top..bottom {
                            let start = ((y + py) * stride + x + left) * 4;
                            for input in rgba[start..start + (right - left) * 4].chunks_exact(4) {
                                for c in 0..3 {
                                    sum[c] += u64::from(input[c]);
                                }
                            }
                        }
                        let count = ((bottom - top) * (right - left)) as u64;
                        for c in 0..3 {
                            pixel[c] = ((sum[c] + count / 2) / count) as u8;
                        }
                    }
                });
        }
        // Codec frames can leave PTS unset. Identical image observations shouldn't vote twice.
        let mut hash = DefaultHasher::new();
        fingerprint(&rgb, &mut hash);
        if record(&mut self.rgb_seen, hash.finish(), self.frames.len(), |i| {
            self.frames[i].rgb() == Some(&rgb)
        }) {
            self.frames.push(Frame::new(w, h, rgb)?);
        }
        Ok(())
    }

    /// Bound coarse detections in each tile. Neighbouring marks in a tile share a
    /// native-resolution pass. A fixed amount of padding lets fine strokes extend
    /// beyond the coarse selection and overlaps boxes on opposite sides of a seam.
    pub fn refinements(
        &self,
        selected: &[u8],
        size: [usize; 2],
        samples: usize,
        budget: usize,
        check: Check<'_>,
    ) -> Result<Vec<Vec<Self>>, String> {
        let [x, y, w, h] = self.region;
        // Reserve the padding on both sides and room for one coarse pixel straddling a tile edge.
        let side = (budget / (samples * self.sample_bytes() + 112)).isqrt();
        let tile = side
            .saturating_sub(2 * (HALO + REFINE_PAD) + self.scale)
            .min(1024);
        if tile == 0 {
            return Err(
                "Detection needs more memory: reduce the detection area or frame count".into(),
            );
        }
        let cols = w.div_ceil(tile);
        let mut bounds = vec![None::<Region>; cols * h.div_ceil(tile)];
        let [lx, ly, lw, _] = self.local_region();
        for (row, flags) in selected.chunks_exact(lw).enumerate() {
            check()?;
            let top = self.context[1] + (ly + row) * self.scale;
            for (col, _) in flags
                .iter()
                .enumerate()
                .filter(|(_, selected)| **selected != 0)
            {
                let left = self.context[0] + (lx + col) * self.scale;
                let tile_x = (left.max(x) - x) / tile;
                let tile_y = (top.max(y) - y) / tile;
                let right = (left + self.scale).min(x + w);
                let bottom = (top + self.scale).min(y + h);
                let entry =
                    bounds[tile_y * cols + tile_x].get_or_insert([left, top, right, bottom]);
                entry[0] = entry[0].min(left);
                entry[1] = entry[1].min(top);
                entry[2] = entry[2].max(right);
                entry[3] = entry[3].max(bottom);
            }
        }
        let mut batches = Vec::new();
        let mut batch = Vec::new();
        let mut bytes = 0;
        for [left, top, right, bottom] in bounds.into_iter().flatten() {
            let left = left.saturating_sub(REFINE_PAD).max(x);
            let top = top.saturating_sub(REFINE_PAD).max(y);
            let right = (right + REFINE_PAD).min(x + w);
            let bottom = (bottom + REFINE_PAD).min(y + h);
            let search = Self::with_native(
                size,
                [left, top, right - left, bottom - top],
                samples,
                budget,
                self.native,
            );
            debug_assert_eq!(search.scale, 1);
            let cost = search.bytes(samples);
            if bytes + cost > budget {
                batches.push(std::mem::take(&mut batch));
                bytes = 0;
            }
            bytes += cost;
            batch.push(search);
        }
        if !batch.is_empty() {
            batches.push(batch);
        }
        Ok(batches)
    }
}

// Sample across the window so indexing new observations stays cheap on large images.
// The bucket still checks exact equality, including unsampled pixels.
pub(super) fn fingerprint<T: Hash>(pixels: &[T], hash: &mut impl Hasher) {
    pixels.len().hash(hash);
    for band in pixels.chunks(pixels.len().div_ceil(64).max(1)) {
        band[..band.len().min(4)].hash(hash);
    }
}

fn record(
    seen: &mut HashMap<u64, Vec<usize>>, hash: u64, next: usize, equal: impl Fn(usize) -> bool,
) -> bool {
    match seen.entry(hash) {
        Entry::Vacant(slot) => {
            slot.insert(vec![next]);
        }
        Entry::Occupied(mut slot) => {
            if slot.get().iter().copied().any(equal) {
                return false;
            }
            slot.get_mut().push(next);
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_collisions_keep_distinct_frames_and_drop_exact_repeats() {
        let mut search = Search::new([32, 16], [0, 0, 32, 16], 8, DETECTOR_BUDGET);
        let mut rgba = vec![0; 32 * 16 * 4];
        search.add(&rgba, 32, [0, 0]).unwrap();
        rgba[8] = 1; // RGB index 6, outside the sampled positions in the first band.
        search.add(&rgba, 32, [0, 0]).unwrap();
        search.add(&rgba, 32, [0, 0]).unwrap();
        assert_eq!(search.frames.len(), 2);
        assert_eq!(search.rgb_seen.len(), 1);
    }

    #[test]
    fn large_searches_fit_the_budget_and_refine_at_native_resolution() {
        let size = [3840, 2160];
        let region = [1, 1, size[0] - 2, size[1] - 2];
        assert_eq!(Search::new(size, region, 16, DETECTOR_BUDGET).scale, 1);
        assert_eq!(Search::new(size, region, 64, DETECTOR_BUDGET).scale, 2);
        let search = Search::new(size, region, 512, DETECTOR_BUDGET);
        assert_eq!(search.scale, 4);
        assert!(search.bytes(512) <= DETECTOR_BUDGET);
        let scout_count = search.scout_count(512, DETECTOR_BUDGET);
        assert!((16..512).contains(&scout_count));
        assert_eq!(
            Search::new(size, region, scout_count, DETECTOR_BUDGET).scale,
            2
        );
        let [_, _, w, h] = search.local_region();
        let selected = vec![255; w * h];
        let batches = search
            .refinements(&selected, size, 512, DETECTOR_BUDGET, &|| Ok(()))
            .unwrap();
        assert!(batches.len() > 1);
        for batch in &batches {
            assert!(batch.iter().map(|search| search.bytes(512)).sum::<usize>() <= DETECTOR_BUDGET);
            for search in batch {
                let [x, y, w, h] = search.region;
                assert_eq!(search.scale, 1);
                assert!(x >= 1 && y >= 1 && x + w <= size[0] - 1 && y + h <= size[1] - 1);
            }
        }
        assert_eq!(&batches[0][0].region[..2], &[1, 1]);
    }
}
