//! Bounded sampling windows for full-resolution and coarse-to-fine searches.
use super::{Check, Frame};
use rayon::prelude::*;

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
}

impl Search {
    pub fn new(size: [usize; 2], region: Region, samples: usize, budget: usize) -> Self {
        let mut search = Self {
            region,
            frames: Vec::new(),
            context: Self::context(size, region),
            scale: 1,
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

    fn bytes(&self, samples: usize) -> usize {
        let [w, h] = self.sample_size();
        w.saturating_mul(h).saturating_mul(samples * 3 + 112)
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
        let affordable = (budget / half_area).saturating_sub(112) / 3;
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
        if !self.frames.iter().any(|frame| frame.rgb == rgb) {
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
        let side = (budget / (samples * 3 + 112)).isqrt();
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
            for (col, _) in flags.iter().enumerate().filter(|(_, selected)| **selected != 0) {
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
            let search = Self::new(
                size,
                [left, top, right - left, bottom - top],
                samples,
                budget,
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

#[cfg(test)]
mod tests {
    use super::*;

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
